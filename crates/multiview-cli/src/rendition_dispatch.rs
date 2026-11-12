//! Bounded rendition-event join and atomic per-output AV window admission.
//!
//! A worker tick is not a decoder-time watermark: encoders can produce zero,
//! many or delayed packets. Only an explicit exclusive DTS watermark, or EOF
//! after all delayed packets, closes a window. No output clock waits here.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::binding_delivery::WindowDelivery;
use crate::rendition_workers::{DispatchEvent, DtsWatermark};
use multiview_core::time::Rational;
use multiview_ffmpeg::{EncodedPacket, StreamKind};
use multiview_output::batch::OfferError;

const MAX_STAGED_WINDOWS: usize = 16;
const MAX_STAGED_PACKETS: usize = 512;
const MAX_STAGED_BYTES: usize = 16 * 1024 * 1024;
const STAGE_DEADLINE: Duration = Duration::from_secs(2);
/// Live (non-retired) dynamic video keys at once.
const MAX_DYNAMIC_KEYS: usize = 8;
/// Hard cap on dynamic slots. A retired slot no binding references any more
/// is reused (fully reset) by the next `add_video_key`, so key churn never
/// grows the stage vectors and never exhausts this cap; it only bounds the
/// pathological case of retired slots still referenced by a binding that has
/// not yet been reconciled. The live cap above is the effective bound.
const MAX_DYNAMIC_SLOTS: usize = 64;
/// Per-binding FIFO of assembled windows waiting for their video part.
const MAX_PENDING_WINDOWS: usize = 4;
const MAX_MOVE_OUTCOMES: usize = 64;

/// Per-key "is this packet a self-contained random-access point that may
/// start a rendition on another output" predicate. Injected, never parsed here.
pub(crate) type MoveGate = Box<dyn Fn(&EncodedPacket) -> bool + Send>;

/// Per-key "the in-band decoder configuration of this RAP" extractor
/// (SPS/PPS/VPS bytes, or the MPEG-2 sequence header). `Absent` = none in
/// band; `Rejected` = malformed (always treated as a change at a switch).
/// Injected, never parsed here. Consulted on keyframes only.
pub(crate) type ParameterSets = Box<dyn Fn(&EncodedPacket) -> RapParameterSets + Send>;
pub(crate) use multiview_ffmpeg::idr::RapParameterSets;

/// The one decoder-configuration decision at the packet that opens a new GOP
/// on an output after a switch (move or fallback): `true` = the output's mux
/// must reset there. `Absent` cut sets splice; `Rejected` is always a change
/// and leaves the output's configuration unknown (the next switch resets
/// too); otherwise the sets are compared and recorded.
fn decide_sets(last: &mut Option<Vec<u8>>, cut: RapParameterSets) -> bool {
    match cut {
        RapParameterSets::Absent => false,
        RapParameterSets::Rejected => {
            *last = None;
            true
        }
        RapParameterSets::Sets(sets) => {
            let changed = last.as_ref() != Some(&sets);
            *last = Some(sets);
            changed
        }
    }
}

/// Why a video move was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveRefusal {
    /// No window in the allowed span carried a gated RAP from the destination
    /// (or the destination failed/retired first).
    EncoderNotReady,
    /// The destination key has no move gate: never move without one.
    NoGate,
}

/// Result of a requested video move or of a failure fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveOutcome {
    Applied {
        output_index: usize,
        key: usize,
        tick: u64,
        /// The cut RAP's parameter sets differ from the ones the output last
        /// carried: a discontinuity was reported to the output before the cut
        /// window (its muxer resets). `false` = a plain splice.
        parameter_sets_changed: bool,
    },
    Refused {
        output_index: usize,
        reason: MoveRefusal,
    },
    /// A pending move was replaced by a newer request.
    Superseded { output_index: usize },
    /// The binding's dynamic key failed/retired; it returned to its startup key.
    Fallback { output_index: usize },
    /// An audio move landed: the output's program-audio slot carries `key`
    /// (`None` = the shared program stage) from packet DTS `from_dts`.
    AudioApplied {
        output_index: usize,
        key: Option<usize>,
        from_dts: i64,
    },
    /// A pending audio move was dropped (its destination failed/retired).
    AudioRefused { output_index: usize },
    /// The output's dynamic audio stage failed/retired: its program-audio
    /// slot returned to the shared program stage at the next packet.
    AudioFallback { output_index: usize },
}

/// Observable drop/failure counters (monotonic, saturating).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DispatchCounters {
    pub dropped_late_dynamic: u64,
    pub dropped_unbound: u64,
    pub dropped_pending: u64,
    pub dynamic_failed: u64,
}

/// Packet DTS time base: seconds per integer DTS unit (`num/den`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct DtsTimeBase {
    pub num: i64,
    pub den: i64,
}

impl DtsTimeBase {
    fn matches(self, actual: Rational) -> bool {
        self.num > 0
            && self.den > 0
            && actual.num > 0
            && actual.den > 0
            && i128::from(self.num) * i128::from(actual.den)
                == i128::from(actual.num) * i128::from(self.den)
    }
    fn nanos(self, dts: i64) -> Option<i128> {
        if self.num <= 0 || self.den <= 0 {
            return None;
        }
        i128::from(dts)
            .checked_mul(i128::from(self.num))?
            .checked_mul(1_000_000_000)?
            .checked_div_euclid(i128::from(self.den))
    }

    // Divide ONCE after composing the two exact rational time bases. Converting
    // DTS to integer nanoseconds first loses one fractional ns at 30000/1001;
    // another floor against the tick boundary misassigns tick N to N-1.
    fn window_index(self, dts: i64, cadence: Rational) -> Option<i128> {
        if self.num <= 0 || self.den <= 0 || cadence.num <= 0 || cadence.den <= 0 {
            return None;
        }
        i128::from(dts)
            .checked_mul(i128::from(self.num))?
            .checked_mul(i128::from(cadence.num))?
            .checked_div_euclid(i128::from(self.den).checked_mul(i128::from(cadence.den))?)
    }
}

/// One configured output binding. `video_key` and `audio_tracks` index the
/// already-open encoder snapshots; no encoder opens inside the adapter.
pub(crate) struct BindingConfig {
    pub output_index: usize,
    pub video_key: Option<usize>,
    pub audio_tracks: Vec<usize>,
}

/// Immutable encoder and output topology for one configuration generation.
pub(crate) struct DispatchConfig {
    pub video_count: usize,
    pub audio_count: usize,
    pub video_time_base: Vec<DtsTimeBase>,
    pub audio_time_base: Vec<DtsTimeBase>,
    pub bindings: Vec<BindingConfig>,
    /// First worker tick admitted in this generation, normally zero.
    pub first_tick: u64,
    /// Exact rational program frames per second, never truncated to a fixed tick.
    pub cadence: Rational,
    /// Per base video key move gate. Empty = none; otherwise `len == video_count`.
    pub video_move_gates: Vec<Option<MoveGate>>,
}

/// An invalid worker event, excessive stage, or missed watchdog deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchError {
    InvalidConfig,
    InvalidEvent,
    MissingDts,
    WatermarkReversed,
    LatePacket,
    StagingOverflow,
    /// `av_packet_ref` failed; no partial AV window may be admitted.
    PacketReference,
    WatchdogExpired,
    /// A binding rejected a whole window; its delivery owner retains details.
    Admission {
        output_index: usize,
    },
}

struct Window {
    index: i128,
    first_seen: Instant,
    packets: Vec<Arc<EncodedPacket>>,
    bytes: usize,
}
struct StreamStage {
    windows: VecDeque<Window>,
    watermark: Option<i128>,
    finished: bool,
    staged_packets: usize,
    staged_bytes: usize,
    last_dts: Option<i128>,
    first_unproved: Option<Instant>,
}
impl StreamStage {
    fn new() -> Self {
        Self {
            windows: VecDeque::new(),
            watermark: None,
            finished: false,
            staged_packets: 0,
            staged_bytes: 0,
            last_dts: None,
            first_unproved: None,
        }
    }
    fn put(
        &mut self,
        packet: EncodedPacket,
        time_base: DtsTimeBase,
        cadence: Rational,
        now: Instant,
    ) -> Result<(), DispatchError> {
        let dts = packet
            .dts()
            .and_then(|value| time_base.nanos(value))
            .ok_or(DispatchError::MissingDts)?;
        if self.watermark.is_some_and(|bound| dts < bound)
            || self.last_dts.is_some_and(|last| dts < last)
        {
            return Err(DispatchError::LatePacket);
        }
        let exact_index = time_base
            .window_index(packet.dts().ok_or(DispatchError::MissingDts)?, cadence)
            .ok_or(DispatchError::InvalidEvent)?;
        // An AAC encoder may emit ONE bounded negative priming frame before
        // sample zero. Preserve its signed DTS for muxing; only its atomic
        // admission window is zero. Never hide later negative or video DTS.
        let index = if exact_index < 0 {
            if packet.kind() != StreamKind::Video
                && self.last_dts.is_none()
                && packet.dts().is_some_and(|raw| raw >= -1024)
            {
                0
            } else {
                return Err(DispatchError::LatePacket);
            }
        } else {
            exact_index
        };
        if self.windows.back().is_some_and(|w| index < w.index) {
            return Err(DispatchError::LatePacket);
        }
        let len = packet.len();
        let packets = self
            .staged_packets
            .checked_add(1)
            .ok_or(DispatchError::StagingOverflow)?;
        let bytes = self
            .staged_bytes
            .checked_add(len)
            .ok_or(DispatchError::StagingOverflow)?;
        if packets > MAX_STAGED_PACKETS || bytes > MAX_STAGED_BYTES {
            return Err(DispatchError::StagingOverflow);
        }
        if self.windows.back().is_none_or(|w| w.index != index) {
            if self.windows.len() == MAX_STAGED_WINDOWS {
                return Err(DispatchError::StagingOverflow);
            }
            self.windows.push_back(Window {
                index,
                first_seen: now,
                packets: Vec::new(),
                bytes: 0,
            });
        }
        let Some(window) = self.windows.back_mut() else {
            return Err(DispatchError::InvalidEvent);
        };
        window.bytes = window
            .bytes
            .checked_add(len)
            .ok_or(DispatchError::StagingOverflow)?;
        window.packets.push(Arc::new(packet));
        self.last_dts = Some(dts);
        self.staged_packets = packets;
        self.staged_bytes = bytes;
        Ok(())
    }
    fn advance(&mut self, watermark: Option<DtsWatermark>) -> Result<(), DispatchError> {
        if let Some(bound) = watermark.map(|w| w.complete_before_ns) {
            if self.watermark.is_some_and(|prior| bound < prior) {
                return Err(DispatchError::WatermarkReversed);
            }
            self.watermark = Some(bound);
            self.first_unproved = None;
        }
        Ok(())
    }
    fn complete(&self, end: i128) -> bool {
        // Exclusive proof must reach the NEXT window's first nanosecond.
        self.finished || self.watermark.is_some_and(|bound| bound >= end)
    }
    fn take(&mut self, index: i128) -> Vec<Arc<EncodedPacket>> {
        if self.windows.front().is_some_and(|w| w.index == index) {
            if let Some(window) = self.windows.pop_front() {
                self.staged_packets -= window.packets.len();
                self.staged_bytes -= window.bytes;
                return window.packets;
            }
        }
        Vec::new()
    }
    fn peek(&self, index: i128) -> Option<&Window> {
        self.windows.iter().find(|w| w.index == index)
    }
    /// Drop staged windows older than `bound`; returns the packets dropped.
    fn prune_before(&mut self, bound: i128) -> usize {
        let mut dropped = 0;
        while self.windows.front().is_some_and(|w| w.index < bound) {
            if let Some(window) = self.windows.pop_front() {
                self.staged_packets = self.staged_packets.saturating_sub(window.packets.len());
                self.staged_bytes = self.staged_bytes.saturating_sub(window.bytes);
                dropped += window.packets.len();
            }
        }
        dropped
    }
    fn stale(&self, now: Instant) -> bool {
        self.windows.front().is_some_and(|w| {
            now.checked_duration_since(w.first_seen)
                .is_none_or(|age| age > STAGE_DEADLINE)
        }) || self.first_unproved.is_some_and(|since| {
            now.checked_duration_since(since)
                .is_none_or(|age| age > STAGE_DEADLINE)
        })
    }
    fn clear(&mut self) {
        self.windows.clear();
        self.staged_packets = 0;
        self.staged_bytes = 0;
    }
}

struct VideoMeta {
    dynamic: bool,
    retired: bool,
    failed: bool,
    gate: Option<MoveGate>,
    params: Option<ParameterSets>,
    /// Bumped every time a dynamic slot is (re)assigned: a pending move
    /// names `(key, generation)`, so a reused slot never satisfies a move
    /// requested against the key that used to live there.
    generation: u64,
}

#[derive(Clone, Copy)]
struct PendingMove {
    key: usize,
    generation: u64,
    at_tick: i128,
    max_windows: u32,
    failed: u32,
}

struct Entry {
    index: i128,
    end: i128,
    span: Duration,
    /// Video of every base key this binding may use for this window (current,
    /// move destination, fallback). A dynamic key's video is read from its
    /// stage at delivery time instead.
    base: Vec<(usize, Vec<EncodedPacket>)>,
    audio: Vec<EncodedPacket>,
}

/// A dynamic (live-opened, per-output) program-audio stage (ADR-0108 §5.2).
/// Mirrors a dynamic video key: it never gates a base window and fails alone.
struct DynAudio {
    stage: StreamStage,
    retired: bool,
    failed: bool,
    generation: u64,
}

/// A requested move of an output's program-audio slot at sample/DTS `at`
/// (a global AAC grid boundary G): packets with DTS < G come from the old
/// source, packets from G from `key`.
#[derive(Clone, Copy)]
struct AudioMove {
    key: Option<usize>,
    generation: u64,
    at: i64,
}

struct BindingState {
    output_index: usize,
    video_key: Option<usize>,
    audio_tracks: Vec<usize>,
    /// Startup video key, returned to when a dynamic key fails.
    fallback: Option<usize>,
    pending: VecDeque<Entry>,
    mv: Option<PendingMove>,
    /// After a fallback the next delivered window must pass the move gate.
    fallback_gate_pending: bool,
    /// Parameter sets of the last RAP delivered to this output (when its
    /// key has an extractor); `None` = unknown (never seen, or the last scan
    /// was rejected), which a switch to a key with sets treats as a change.
    last_sets: Option<Vec<u8>>,
    /// The dynamic audio stage carrying this output's program-audio slot
    /// (`None` = the shared program stage, today's behaviour).
    audio_key: Option<usize>,
    audio_mv: Option<AudioMove>,
}

/// The dispatcher window of program-audio DTS `at` (the program stage's
/// time base), or `None` without a program-audio stage.
fn audio_window_of(bases: &[DtsTimeBase], cadence: Rational, at: i64) -> Option<i128> {
    bases.first()?.window_index(at, cadence)
}

/// One output's program-audio packets of window `index`, from the stage it
/// subscribes, split at a pending move's grid boundary G: DTS < G from the
/// old source, DTS >= G from the new one (both encoders share the global AAC
/// grid, so nothing is duplicated or missing). Discrete-track packets pass
/// through unchanged after the program slot. With no routed audio this is
/// `base` unchanged (today's window, byte for byte).
fn compose_audio(
    b: &mut BindingState,
    dyn_audio: &[DynAudio],
    outcomes: &mut VecDeque<MoveOutcome>,
    index: i128,
    mv_index: Option<i128>,
    base: Vec<EncodedPacket>,
) -> Result<Vec<EncodedPacket>, DispatchError> {
    let mv = b.audio_mv.filter(|_| mv_index.is_some());
    if b.audio_key.is_none() && mv.is_none() {
        return Ok(base);
    }
    let (mut program, tracks): (Vec<_>, Vec<_>) = base
        .into_iter()
        .partition(|p| p.kind() == StreamKind::Audio);
    let mut take = |key: Option<usize>,
                    keep: &dyn Fn(i64) -> bool|
     -> Result<Vec<EncodedPacket>, DispatchError> {
        let mut out = Vec::new();
        match key {
            None => {
                let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut program)
                    .into_iter()
                    .partition(|p| p.dts().is_some_and(keep));
                program = rest;
                out = mine;
            }
            Some(k) => {
                if let Some(w) = dyn_audio.get(k).and_then(|a| a.stage.peek(index)) {
                    for p in w.packets.iter().filter(|p| p.dts().is_some_and(keep)) {
                        out.push(
                            p.clone_shared_ref()
                                .map_err(|_| DispatchError::PacketReference)?,
                        );
                    }
                }
            }
        }
        Ok(out)
    };
    let mut out = if let Some(m) = mv {
        let mut out = take(b.audio_key, &|dts| dts < m.at)?;
        out.append(&mut take(m.key, &|dts| dts >= m.at)?);
        b.audio_key = m.key;
        b.audio_mv = None;
        push_outcome(
            outcomes,
            MoveOutcome::AudioApplied {
                output_index: b.output_index,
                key: m.key,
                from_dts: m.at,
            },
        );
        out
    } else {
        take(b.audio_key, &|_| true)?
    };
    out.extend(tracks);
    Ok(out)
}

fn push_outcome(outcomes: &mut VecDeque<MoveOutcome>, outcome: MoveOutcome) {
    if outcomes.len() >= MAX_MOVE_OUTCOMES {
        outcomes.pop_front();
    }
    outcomes.push_back(outcome);
}

/// Per-program bounded join. The caller handles returned errors and never
/// labels an output healthy after a rejected event or missing proof.
pub(crate) struct RenditionDispatcher {
    bindings: Vec<BindingState>,
    video_meta: Vec<VideoMeta>,
    base_video: usize,
    counters: DispatchCounters,
    outcomes: VecDeque<MoveOutcome>,
    video: Vec<StreamStage>,
    audio: Vec<StreamStage>,
    video_time_base: Vec<DtsTimeBase>,
    audio_time_base: Vec<DtsTimeBase>,
    cadence: Rational,
    next_window: Option<i128>,
    global_staged_packets: usize,
    global_staged_bytes: usize,
    /// Source of `VideoMeta::generation` for (re)assigned dynamic slots.
    next_key_generation: u64,
    /// Dynamic per-output audio stages (never gate base windows).
    dyn_audio: Vec<DynAudio>,
    failed: bool,
    finished: bool,
}
impl RenditionDispatcher {
    fn tick_boundary(index: i128, cadence: Rational) -> Option<i128> {
        index
            .checked_mul(i128::from(cadence.den))?
            .checked_mul(1_000_000_000)?
            .checked_div(i128::from(cadence.num))
    }
    /// Validate all topology and install strict codec-specific recovery gates.
    pub(crate) fn new(config: DispatchConfig) -> Result<Self, DispatchError> {
        if config.cadence.num <= 0
            || config.cadence.den <= 0
            || i128::from(config.cadence.den) * 1_000_000_000
                > i128::from(config.cadence.num) * 250_000_000
            || config.video_count != config.video_time_base.len()
            || config.video_time_base.iter().any(|tb| {
                i128::from(tb.num).checked_mul(i128::from(config.cadence.num))
                    != i128::from(tb.den).checked_mul(i128::from(config.cadence.den))
            })
            || config.audio_count != config.audio_time_base.len()
            || config.video_count == 0
            || (!config.video_move_gates.is_empty()
                && config.video_move_gates.len() != config.video_count)
            || config.bindings.is_empty()
        {
            return Err(DispatchError::InvalidConfig);
        }
        let mut bindings = Vec::with_capacity(config.bindings.len());
        for binding in config.bindings {
            if binding
                .video_key
                .is_some_and(|idx| idx >= config.video_count)
                || binding
                    .audio_tracks
                    .iter()
                    .any(|&idx| idx >= config.audio_count)
            {
                return Err(DispatchError::InvalidConfig);
            }
            bindings.push(BindingState {
                output_index: binding.output_index,
                video_key: binding.video_key,
                audio_tracks: binding.audio_tracks,
                fallback: binding.video_key,
                pending: VecDeque::new(),
                mv: None,
                fallback_gate_pending: false,
                last_sets: None,
                audio_key: None,
                audio_mv: None,
            });
        }
        let mut gates = config.video_move_gates.into_iter();
        let video_meta = (0..config.video_count)
            .map(|_| VideoMeta {
                dynamic: false,
                retired: false,
                failed: false,
                gate: gates.next().flatten(),
                params: None,
                generation: 0,
            })
            .collect();
        Ok(Self {
            bindings,
            video_meta,
            base_video: config.video_count,
            counters: DispatchCounters::default(),
            outcomes: VecDeque::new(),
            video: (0..config.video_count)
                .map(|_| StreamStage::new())
                .collect(),
            audio: (0..config.audio_count)
                .map(|_| StreamStage::new())
                .collect(),
            video_time_base: config.video_time_base,
            audio_time_base: config.audio_time_base,
            cadence: config.cadence,
            next_window: Some(i128::from(config.first_tick)),
            global_staged_packets: 0,
            global_staged_bytes: 0,
            next_key_generation: 1,
            dyn_audio: Vec::new(),
            failed: false,
            finished: false,
        })
    }
    /// Receive one worker emission, then atomically commit every ready window.
    pub(crate) fn dispatch<D: WindowDelivery>(
        &mut self,
        event: DispatchEvent,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        if self.failed || self.finished {
            return Err(DispatchError::InvalidEvent);
        }
        let dynamic_event = matches!(
            &event,
            DispatchEvent::Video { key_index, .. } | DispatchEvent::FinishVideo { key_index, .. }
                if *key_index >= self.base_video
        );
        // A live (dynamic) key's event skips the GLOBAL budget: it must never
        // fail the run. Its own ceiling is per-stage (`StreamStage::put`:
        // MAX_STAGED_WINDOWS / MAX_STAGED_PACKETS / MAX_STAGED_BYTES = 16 MiB,
        // overflow fails that key alone) times at most MAX_DYNAMIC_KEYS (8)
        // live keys, so the worst case added beside the base budget is
        // 8 x 16 MiB = 128 MiB and 8 x 512 packets, bounded and per key.
        if let Err(error) = if dynamic_event {
            Ok(())
        } else {
            self.check_global_budget(&event)
        } {
            self.fail();
            delivery.abort();
            return Err(error);
        }
        let result = self.apply(event, now).and_then(|()| {
            if self
                .video
                .iter()
                .take(self.base_video)
                .chain(&self.audio)
                .any(|stage| {
                    stage.windows.front().is_some_and(|window| {
                        self.next_window.is_some_and(|next| window.index < next)
                    })
                })
            {
                return Err(DispatchError::LatePacket);
            }
            self.drain(now, delivery)
        });
        if let Err(error) = result {
            if let DispatchError::Admission { output_index, .. } = error {
                // A slow binding already fenced its own queue. Other outputs
                // must continue; surface the fault without global fail().
                let _ = output_index;
            } else {
                self.fail();
                delivery.abort();
            }
            return Err(error);
        }
        Ok(())
    }
    fn check_global_budget(&self, event: &DispatchEvent) -> Result<(), DispatchError> {
        let packets = match event {
            DispatchEvent::Video { packets, .. }
            | DispatchEvent::Audio { packets, .. }
            | DispatchEvent::FinishVideo { packets, .. }
            | DispatchEvent::FinishAudio { packets } => packets,
        };
        let count = self
            .global_staged_packets
            .checked_add(packets.len())
            .ok_or(DispatchError::StagingOverflow)?;
        if count > MAX_STAGED_PACKETS {
            return Err(DispatchError::StagingOverflow);
        }
        let mut bytes = self.global_staged_bytes;
        for packet in packets {
            bytes = bytes
                .checked_add(packet.len())
                .ok_or(DispatchError::StagingOverflow)?;
            if bytes > MAX_STAGED_BYTES {
                return Err(DispatchError::StagingOverflow);
            }
        }
        Ok(())
    }
    fn put_video(
        &mut self,
        index: usize,
        packet: EncodedPacket,
        now: Instant,
    ) -> Result<(), DispatchError> {
        let bytes = packet.len();
        let count = self
            .global_staged_packets
            .checked_add(1)
            .ok_or(DispatchError::StagingOverflow)?;
        let total = self
            .global_staged_bytes
            .checked_add(bytes)
            .ok_or(DispatchError::StagingOverflow)?;
        if count > MAX_STAGED_PACKETS || total > MAX_STAGED_BYTES {
            return Err(DispatchError::StagingOverflow);
        }
        self.video
            .get_mut(index)
            .ok_or(DispatchError::InvalidEvent)?
            .put(
                packet,
                *self
                    .video_time_base
                    .get(index)
                    .ok_or(DispatchError::InvalidEvent)?,
                self.cadence,
                now,
            )?;
        self.global_staged_packets = count;
        self.global_staged_bytes = total;
        Ok(())
    }
    fn put_audio(
        &mut self,
        index: usize,
        packet: EncodedPacket,
        now: Instant,
    ) -> Result<(), DispatchError> {
        let bytes = packet.len();
        let count = self
            .global_staged_packets
            .checked_add(1)
            .ok_or(DispatchError::StagingOverflow)?;
        let total = self
            .global_staged_bytes
            .checked_add(bytes)
            .ok_or(DispatchError::StagingOverflow)?;
        if count > MAX_STAGED_PACKETS || total > MAX_STAGED_BYTES {
            return Err(DispatchError::StagingOverflow);
        }
        self.audio
            .get_mut(index)
            .ok_or(DispatchError::InvalidEvent)?
            .put(
                packet,
                *self
                    .audio_time_base
                    .get(index)
                    .ok_or(DispatchError::InvalidEvent)?,
                self.cadence,
                now,
            )?;
        self.global_staged_packets = count;
        self.global_staged_bytes = total;
        Ok(())
    }
    #[allow(clippy::too_many_lines)] // One exhaustive typed worker-event state transition; splitting hides the protocol invariants.
    fn apply(&mut self, event: DispatchEvent, now: Instant) -> Result<(), DispatchError> {
        match event {
            DispatchEvent::Video {
                key_index,
                tick,
                timebase,
                packets,
                watermark,
            } => {
                if key_index >= self.base_video {
                    return self.apply_dynamic_video(
                        key_index,
                        Some(timebase),
                        packets,
                        watermark,
                        false,
                        now,
                    );
                }
                if self
                    .video_time_base
                    .get(key_index)
                    .is_none_or(|tb| !tb.matches(timebase))
                {
                    return Err(DispatchError::InvalidEvent);
                }
                if self.video.get(key_index).is_none_or(|s| s.finished) {
                    return Err(DispatchError::InvalidEvent);
                }
                let _ = tick; // Tick alone does not close delayed DTS.
                for p in packets {
                    if p.kind() != StreamKind::Video {
                        return Err(DispatchError::InvalidEvent);
                    }
                    self.put_video(key_index, p, now)?;
                }
                let stage = self
                    .video
                    .get_mut(key_index)
                    .ok_or(DispatchError::InvalidEvent)?;
                if watermark.is_none() {
                    stage.first_unproved.get_or_insert(now);
                }
                stage.advance(watermark)?;
            }
            DispatchEvent::Audio {
                tick,
                timebases,
                packets,
                watermarks,
            } => {
                if timebases.len() != self.audio_time_base.len()
                    || self
                        .audio_time_base
                        .iter()
                        .zip(&timebases)
                        .any(|(tb, &actual)| !tb.matches(actual))
                {
                    return Err(DispatchError::InvalidEvent);
                }
                let _ = tick;
                if watermarks.len() != self.audio.len() {
                    return Err(DispatchError::InvalidEvent);
                }
                for p in packets {
                    let index = match p.kind() {
                        StreamKind::Audio => 0,
                        StreamKind::AudioTrack(n) => usize::from(n) + 1,
                        _ => return Err(DispatchError::InvalidEvent),
                    };
                    if self.audio.get(index).is_none_or(|s| s.finished) {
                        return Err(DispatchError::InvalidEvent);
                    }
                    self.put_audio(index, p, now)?;
                }
                // Shared audio event covers ALL tracks; absence of one track
                // still requires its own proven watermark, not packet presence.
                for (stage, watermark) in self.audio.iter_mut().zip(watermarks) {
                    if watermark.is_none() {
                        stage.first_unproved.get_or_insert(now);
                    }
                    stage.advance(watermark)?;
                }
            }
            DispatchEvent::FinishVideo { key_index, packets } => {
                if key_index >= self.base_video {
                    return self.apply_dynamic_video(key_index, None, packets, None, true, now);
                }
                if self.video.get(key_index).is_none_or(|s| s.finished) {
                    return Err(DispatchError::InvalidEvent);
                }
                for p in packets {
                    if p.kind() != StreamKind::Video {
                        return Err(DispatchError::InvalidEvent);
                    }
                    self.put_video(key_index, p, now)?;
                }
                let stage = self
                    .video
                    .get_mut(key_index)
                    .ok_or(DispatchError::InvalidEvent)?;
                stage.finished = true;
                stage.first_unproved = None;
            }
            DispatchEvent::FinishAudio { packets } => {
                for p in packets {
                    let index = match p.kind() {
                        StreamKind::Audio => 0,
                        StreamKind::AudioTrack(n) => usize::from(n) + 1,
                        _ => return Err(DispatchError::InvalidEvent),
                    };
                    if self.audio.get(index).is_none_or(|s| s.finished) {
                        return Err(DispatchError::InvalidEvent);
                    }
                    self.put_audio(index, p, now)?;
                }
                for stage in &mut self.audio {
                    if stage.finished {
                        return Err(DispatchError::InvalidEvent);
                    }
                    stage.finished = true;
                    stage.first_unproved = None;
                }
            }
        }
        Ok(())
    }
    fn apply_dynamic_video(
        &mut self,
        key: usize,
        timebase: Option<Rational>,
        packets: Vec<EncodedPacket>,
        watermark: Option<DtsWatermark>,
        finish: bool,
        now: Instant,
    ) -> Result<(), DispatchError> {
        let Some(meta) = self.video_meta.get(key) else {
            return Err(DispatchError::InvalidEvent);
        };
        if meta.retired || meta.failed {
            self.counters.dropped_unbound = self.counters.dropped_unbound.saturating_add(1);
            return Ok(());
        }
        let tb = *self
            .video_time_base
            .get(key)
            .ok_or(DispatchError::InvalidEvent)?;
        let cadence = self.cadence;
        let result = (|| -> Result<(), DispatchError> {
            let stage = self.video.get_mut(key).ok_or(DispatchError::InvalidEvent)?;
            if stage.finished || timebase.is_some_and(|t| !tb.matches(t)) {
                return Err(DispatchError::InvalidEvent);
            }
            for p in packets {
                if p.kind() != StreamKind::Video {
                    return Err(DispatchError::InvalidEvent);
                }
                stage.put(p, tb, cadence, now)?;
            }
            if finish {
                stage.finished = true;
                stage.first_unproved = None;
            } else {
                if watermark.is_none() {
                    stage.first_unproved.get_or_insert(now);
                }
                stage.advance(watermark)?;
            }
            Ok(())
        })();
        if result.is_err() {
            // A live key's fault is its own: never the program's.
            self.fail_dynamic(key);
        }
        Ok(())
    }
    fn fail_dynamic(&mut self, key: usize) {
        if let Some(meta) = self.video_meta.get_mut(key) {
            if !meta.failed {
                meta.failed = true;
                self.counters.dynamic_failed = self.counters.dynamic_failed.saturating_add(1);
            }
        }
        if let Some(stage) = self.video.get_mut(key) {
            stage.clear();
        }
    }
    /// Bindings on a failed/retired dynamic key return to their startup key
    /// (gated on a RAP); moves to such a key are refused.
    fn reconcile(&mut self) {
        for b in &mut self.bindings {
            if let Some(m) = b.mv {
                if self
                    .video_meta
                    .get(m.key)
                    .is_none_or(|x| x.failed || x.retired || x.generation != m.generation)
                {
                    push_outcome(
                        &mut self.outcomes,
                        MoveOutcome::Refused {
                            output_index: b.output_index,
                            reason: MoveRefusal::EncoderNotReady,
                        },
                    );
                    b.mv = None;
                }
            }
            if let (Some(k), Some(fb)) = (b.video_key, b.fallback) {
                if self
                    .video_meta
                    .get(k)
                    .is_some_and(|x| x.failed || x.retired)
                {
                    b.video_key = Some(fb);
                    b.fallback_gate_pending = true;
                    push_outcome(
                        &mut self.outcomes,
                        MoveOutcome::Fallback {
                            output_index: b.output_index,
                        },
                    );
                }
            }
        }
    }
    fn fail_stale_dynamic(&mut self, now: Instant) {
        for k in self.base_video..self.video.len() {
            if self.video.get(k).is_some_and(|s| s.stale(now)) {
                self.fail_dynamic(k);
            }
        }
        for k in 0..self.dyn_audio.len() {
            if self.dyn_audio.get(k).is_some_and(|a| a.stage.stale(now)) {
                self.fail_dynamic_audio(k);
            }
        }
    }
    fn fail_dynamic_audio(&mut self, key: usize) {
        if let Some(a) = self.dyn_audio.get_mut(key) {
            if !a.failed {
                a.failed = true;
                self.counters.dynamic_failed = self.counters.dynamic_failed.saturating_add(1);
            }
            a.stage.clear();
        }
    }
    fn audio_usable(&self, key: Option<usize>, generation: u64) -> bool {
        key.is_none_or(|k| {
            self.dyn_audio
                .get(k)
                .is_some_and(|a| !a.failed && !a.retired && a.generation == generation)
        })
    }
    /// Bindings on a failed/retired dynamic audio stage return to the shared
    /// program stage; audio moves to such a stage are dropped.
    fn reconcile_audio(&mut self) {
        let dyn_audio = &self.dyn_audio;
        let usable = |key: Option<usize>| {
            key.is_none_or(|k| dyn_audio.get(k).is_some_and(|a| !a.failed && !a.retired))
        };
        for b in &mut self.bindings {
            if let Some(m) = b.audio_mv {
                let gen_ok = m.key.is_none_or(|k| {
                    dyn_audio
                        .get(k)
                        .is_some_and(|a| a.generation == m.generation)
                });
                if !usable(m.key) || !gen_ok {
                    b.audio_mv = None;
                    push_outcome(
                        &mut self.outcomes,
                        MoveOutcome::AudioRefused {
                            output_index: b.output_index,
                        },
                    );
                }
            }
            if !usable(b.audio_key) {
                b.audio_key = None;
                push_outcome(
                    &mut self.outcomes,
                    MoveOutcome::AudioFallback {
                        output_index: b.output_index,
                    },
                );
            }
        }
    }
    fn base_stale(&self, now: Instant) -> bool {
        self.video
            .iter()
            .take(self.base_video)
            .chain(&self.audio)
            .any(|s| s.stale(now))
    }
    fn drain<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        if self.base_stale(now) {
            return Err(DispatchError::WatchdogExpired);
        }
        self.fail_stale_dynamic(now);
        self.reconcile();
        self.reconcile_audio();
        let early = self.pump(now, delivery)?;
        let result = self.drain_base(now, delivery);
        self.prune_dynamic();
        result?;
        early.map_or(Ok(()), Err)
    }
    /// Free dynamic windows no binding can still need.
    fn prune_dynamic(&mut self) {
        let next = self.next_window.unwrap_or(i128::MAX);
        for k in self.base_video..self.video.len() {
            let mut floor = next;
            for b in &self.bindings {
                if b.video_key == Some(k) || b.mv.is_some_and(|m| m.key == k) {
                    if let Some(head) = b.pending.front() {
                        floor = floor.min(head.index);
                    }
                }
            }
            if let Some(stage) = self.video.get_mut(k) {
                let dropped = stage.prune_before(floor);
                self.counters.dropped_late_dynamic = self
                    .counters
                    .dropped_late_dynamic
                    .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            }
        }
        for k in 0..self.dyn_audio.len() {
            let mut floor = next;
            for b in &self.bindings {
                if b.audio_key == Some(k) || b.audio_mv.is_some_and(|m| m.key == Some(k)) {
                    if let Some(head) = b.pending.front() {
                        floor = floor.min(head.index);
                    }
                }
            }
            if let Some(a) = self.dyn_audio.get_mut(k) {
                let dropped = a.stage.prune_before(floor);
                self.counters.dropped_late_dynamic = self
                    .counters
                    .dropped_late_dynamic
                    .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
            }
        }
    }
    fn pump<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<Option<DispatchError>, DispatchError> {
        let mut first = None;
        // A dynamic audio stage that holds a binding's full pending FIFO has
        // stalled: it fails alone and the output returns to the program
        // stage (whole AAC frames, so the return is on a grid boundary).
        let mut stalled = Vec::new();
        for b in &self.bindings {
            let Some(head) = b.pending.front() else {
                continue;
            };
            if b.pending.len() < MAX_PENDING_WINDOWS {
                continue;
            }
            let landing = b.audio_mv.filter(|m| {
                audio_window_of(&self.audio_time_base, self.cadence, m.at)
                    .is_some_and(|at| at <= head.index)
            });
            for k in [b.audio_key, landing.and_then(|m| m.key)]
                .into_iter()
                .flatten()
            {
                if self
                    .dyn_audio
                    .get(k)
                    .is_some_and(|a| !a.failed && !a.retired && !a.stage.complete(head.end))
                {
                    stalled.push(k);
                }
            }
        }
        if !stalled.is_empty() {
            for k in stalled {
                self.fail_dynamic_audio(k);
            }
            self.reconcile_audio();
        }
        for bi in 0..self.bindings.len() {
            if let Some(error) = self.pump_binding(bi, now, delivery)? {
                first.get_or_insert(error);
            }
        }
        Ok(first)
    }
    /// Deliver, in order, every pending window of one binding whose video part
    /// is available. Resolves video moves and post-fallback RAP gating.
    #[allow(clippy::too_many_lines)] // One in-order per-binding delivery state machine.
    fn pump_binding<D: WindowDelivery>(
        &mut self,
        bi: usize,
        now: Instant,
        delivery: &mut D,
    ) -> Result<Option<DispatchError>, DispatchError> {
        let mut first_err = None;
        let Some(b) = self.bindings.get_mut(bi) else {
            return Ok(None);
        };
        while let Some(head) = b.pending.front() {
            let end = head.end;
            if let Some(k) = b.video_key {
                if self.video_meta.get(k).is_some_and(|m| m.dynamic)
                    && !self.video.get(k).is_some_and(|s| s.complete(end))
                {
                    break;
                }
            }
            // A dynamic audio stage this window reads (current, or a move
            // landing in it) must have proven the window complete. A stalled
            // stage holds only this binding (bounded by MAX_PENDING_WINDOWS
            // and the stage watchdog), never a base window.
            let mv_index = b
                .audio_mv
                .and_then(|m| audio_window_of(&self.audio_time_base, self.cadence, m.at))
                .filter(|&at| at <= head.index);
            let needs = [
                b.audio_key,
                mv_index.and_then(|_| b.audio_mv.and_then(|m| m.key)),
            ];
            if needs.into_iter().flatten().any(|k| {
                self.dyn_audio
                    .get(k)
                    .is_some_and(|a| !a.failed && !a.retired && !a.stage.complete(end))
            }) {
                break;
            }
            let Some(mut entry) = b.pending.pop_front() else {
                break;
            };
            let index = entry.index;
            let mut applied = false;
            // Leading video packets of the cut window before its first gated
            // RAP: never delivered after a switch (audio is kept).
            let mut skip = 0_usize;
            if let Some(mut m) = b.mv {
                if index >= m.at_tick {
                    let usable = self
                        .video_meta
                        .get(m.key)
                        .is_some_and(|x| !x.failed && !x.retired && x.generation == m.generation);
                    if usable {
                        let dest_dynamic = self.video_meta.get(m.key).is_some_and(|x| x.dynamic);
                        // The gate passes on ANY video packet of the window;
                        // the delivered window starts at the first that does.
                        let gated_rap = |gate: &Option<MoveGate>| -> Option<usize> {
                            let g = gate.as_ref()?;
                            if dest_dynamic {
                                if !self.video.get(m.key).is_some_and(|s| s.complete(end)) {
                                    return None;
                                }
                                self.video
                                    .get(m.key)
                                    .and_then(|s| s.peek(index))
                                    .and_then(|w| w.packets.iter().position(|p| g(p)))
                            } else {
                                entry
                                    .base
                                    .iter()
                                    .find(|(k, _)| *k == m.key)
                                    .and_then(|(_, v)| v.iter().position(g))
                            }
                        };
                        let pass = self.video_meta.get(m.key).and_then(|x| gated_rap(&x.gate));
                        if let Some(at) = pass {
                            skip = at;
                            // Compare the cut RAP's decoder configuration with
                            // the output's: a change resets the output's mux.
                            let cut_sets = self
                                .video_meta
                                .get(m.key)
                                .and_then(|x| {
                                    let f = x.params.as_ref()?;
                                    if dest_dynamic {
                                        self.video
                                            .get(m.key)
                                            .and_then(|s| s.peek(index))
                                            .and_then(|w| w.packets.get(at))
                                            .map(|p| f(p))
                                    } else {
                                        entry
                                            .base
                                            .iter()
                                            .find(|(k, _)| *k == m.key)
                                            .and_then(|(_, v)| v.get(at))
                                            .map(f)
                                    }
                                })
                                .unwrap_or(RapParameterSets::Absent);
                            let changed = decide_sets(&mut b.last_sets, cut_sets);
                            if changed {
                                // Not a loss: the cut window opens at a strict
                                // RAP. A loss here would arm the queue's RAP
                                // gate and flush its healthy backlog.
                                delivery.mark_discontinuity(
                                    b.output_index,
                                    OfferError::DecoderConfigChanged,
                                );
                            }
                            b.video_key = Some(m.key);
                            push_outcome(
                                &mut self.outcomes,
                                MoveOutcome::Applied {
                                    output_index: b.output_index,
                                    key: m.key,
                                    tick: u64::try_from(index).unwrap_or(u64::MAX),
                                    parameter_sets_changed: changed,
                                },
                            );
                            b.mv = None;
                            applied = true;
                        } else {
                            m.failed = m.failed.saturating_add(1);
                            if m.failed >= m.max_windows {
                                push_outcome(
                                    &mut self.outcomes,
                                    MoveOutcome::Refused {
                                        output_index: b.output_index,
                                        reason: MoveRefusal::EncoderNotReady,
                                    },
                                );
                                b.mv = None;
                            } else {
                                b.mv = Some(m);
                            }
                        }
                    } else {
                        push_outcome(
                            &mut self.outcomes,
                            MoveOutcome::Refused {
                                output_index: b.output_index,
                                reason: MoveRefusal::EncoderNotReady,
                            },
                        );
                        b.mv = None;
                    }
                }
            }
            let cur = b.video_key;
            // This window opens a new GOP after a switch: its sets were
            // decided (and recorded) once, at the opening packet.
            let mut switched = applied;
            let mut packets: Vec<EncodedPacket> = Vec::new();
            if let Some(k) = cur {
                if self.video_meta.get(k).is_some_and(|m| m.dynamic) {
                    if let Some(w) = self.video.get(k).and_then(|s| s.peek(index)) {
                        for p in &w.packets {
                            packets.push(
                                p.clone_shared_ref()
                                    .map_err(|_| DispatchError::PacketReference)?,
                            );
                        }
                    }
                } else if let Some(pos) = entry.base.iter().position(|(bk, _)| *bk == k) {
                    packets = entry.base.swap_remove(pos).1;
                }
            }
            if b.fallback_gate_pending && cur.is_some() {
                let gate = cur
                    .and_then(|k| self.video_meta.get(k))
                    .and_then(|m| m.gate.as_ref());
                let rap = if applied {
                    Some(skip)
                } else {
                    match gate {
                        None => Some(0),
                        Some(g) => packets.iter().position(g),
                    }
                };
                if let Some(at) = rap {
                    skip = at;
                    b.fallback_gate_pending = false;
                    if !applied {
                        // The fallback's GOP-opening packet: the same single
                        // decision a move makes (a move decided above).
                        let cut = cur
                            .and_then(|k| self.video_meta.get(k))
                            .and_then(|m| m.params.as_ref())
                            .and_then(|f| packets.get(at).map(f))
                            .unwrap_or(RapParameterSets::Absent);
                        if decide_sets(&mut b.last_sets, cut) {
                            delivery.mark_discontinuity(
                                b.output_index,
                                OfferError::DecoderConfigChanged,
                            );
                        }
                    }
                    switched = true;
                } else {
                    // Never a mid-GOP picture after a switch: the whole window is lost.
                    delivery.report_loss(b.output_index, OfferError::ProducerLoss);
                    self.counters.dropped_pending = self.counters.dropped_pending.saturating_add(1);
                    continue;
                }
            }
            if skip > 0 {
                packets.drain(..skip.min(packets.len()));
            }
            // Steady state: track the CURRENT key's configuration from its own
            // RAPs so the next switch compares against what the output carries.
            // Never on a switch window (decided above, never overridden).
            if let Some(f) = cur
                .and_then(|k| self.video_meta.get(k))
                .and_then(|m| m.params.as_ref())
                .filter(|_| !switched)
            {
                for p in packets.iter().filter(|p| p.is_keyframe()) {
                    match f(p) {
                        RapParameterSets::Absent => {}
                        RapParameterSets::Rejected => b.last_sets = None,
                        RapParameterSets::Sets(sets) => {
                            if b.last_sets.as_ref() != Some(&sets) {
                                b.last_sets = Some(sets);
                            }
                        }
                    }
                }
            }
            let mut audio = compose_audio(
                b,
                &self.dyn_audio,
                &mut self.outcomes,
                index,
                mv_index,
                std::mem::take(&mut entry.audio),
            )?;
            packets.append(&mut audio);
            if delivery
                .offer_window(
                    b.output_index,
                    delivery.generation(b.output_index),
                    packets,
                    entry.span,
                    now,
                )
                .is_err()
            {
                first_err.get_or_insert(DispatchError::Admission {
                    output_index: b.output_index,
                });
            }
        }
        Ok(first_err)
    }
    #[allow(clippy::too_many_lines)] // Atomic whole-window closure and fanout are reviewed as one state transition.
    fn drain_base<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        let bv = self.base_video;
        loop {
            let Some(index) = self.next_window else {
                return Ok(());
            };
            let live: Vec<_> = self
                .video
                .iter()
                .take(bv)
                .chain(&self.audio)
                .filter(|s| !s.finished)
                .collect();
            let min_watermark = if live.iter().all(|s| s.watermark.is_some()) {
                live.iter().filter_map(|s| s.watermark).min()
            } else {
                None
            };
            let max_index = self
                .video
                .iter()
                .take(bv)
                .chain(&self.audio)
                .filter_map(|s| s.windows.back().map(|w| w.index))
                .max();
            if max_index.is_none_or(|last| index > last)
                && self
                    .video
                    .iter()
                    .take(bv)
                    .chain(&self.audio)
                    .all(|s| s.finished)
            {
                self.finished = true;
                return Ok(());
            }
            if max_index.is_none_or(|last| index > last)
                && min_watermark.is_none_or(|bound| {
                    Self::tick_boundary(index, self.cadence).is_none_or(|start| bound <= start)
                })
            {
                return Ok(());
            }
            let end = index
                .checked_add(1)
                .and_then(|i| Self::tick_boundary(i, self.cadence))
                .ok_or(DispatchError::InvalidEvent)?;
            if !self.video.iter().take(bv).all(|stage| stage.complete(end))
                || !self.audio.iter().all(|stage| stage.complete(end))
            {
                return Ok(());
            }
            let video: Vec<Vec<_>> = self
                .video
                .iter_mut()
                .take(bv)
                .map(|s| s.take(index))
                .collect();
            let audio: Vec<Vec<_>> = self.audio.iter_mut().map(|s| s.take(index)).collect();
            let removed_packets = video.iter().chain(&audio).map(Vec::len).sum::<usize>();
            let removed_bytes = video
                .iter()
                .chain(&audio)
                .flat_map(|part| part.iter())
                .map(|packet| packet.len())
                .sum::<usize>();
            self.global_staged_packets -= removed_packets;
            self.global_staged_bytes -= removed_bytes;
            self.next_window = index.checked_add(1);
            let start =
                Self::tick_boundary(index, self.cadence).ok_or(DispatchError::InvalidEvent)?;
            let span = Duration::from_nanos(
                u64::try_from(end.checked_sub(start).ok_or(DispatchError::InvalidEvent)?)
                    .map_err(|_| DispatchError::InvalidEvent)?,
            );
            for binding in &mut self.bindings {
                let mut keys: Vec<usize> = Vec::new();
                for k in [
                    binding.video_key,
                    binding.mv.map(|m| m.key),
                    binding.fallback,
                ]
                .into_iter()
                .flatten()
                {
                    if k < bv && !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                let mut base = Vec::with_capacity(keys.len());
                for k in keys {
                    let mut part = Vec::new();
                    for p in video.get(k).ok_or(DispatchError::InvalidConfig)? {
                        part.push(
                            p.clone_shared_ref()
                                .map_err(|_| DispatchError::PacketReference)?,
                        );
                    }
                    base.push((k, part));
                }
                let mut audio_packets = Vec::new();
                for &a in &binding.audio_tracks {
                    for p in audio.get(a).ok_or(DispatchError::InvalidConfig)? {
                        audio_packets.push(
                            p.clone_shared_ref()
                                .map_err(|_| DispatchError::PacketReference)?,
                        );
                    }
                }
                if binding.pending.len() >= MAX_PENDING_WINDOWS
                    && binding.pending.pop_front().is_some()
                {
                    delivery.report_loss(binding.output_index, OfferError::ProducerLoss);
                    self.counters.dropped_pending = self.counters.dropped_pending.saturating_add(1);
                }
                binding.pending.push_back(Entry {
                    index,
                    end,
                    span,
                    base,
                    audio: audio_packets,
                });
            }
            let first_admission_error = self.pump(now, delivery)?;
            // The dispatcher's COMPLETION is a fact about its own stages and
            // must be recorded even when a binding rejected this window (the
            // rejection is that binding's own explicit loss, reported in its
            // truth line — it must not also corrupt the completion bookkeeping:
            // the last window of a run landing in a full binding previously
            // left `finished` unset and failed the run as "incomplete final
            // window").
            if self
                .video
                .iter()
                .take(bv)
                .chain(&self.audio)
                .all(|s| s.finished && s.windows.is_empty())
            {
                self.finished = true;
            }
            if let Some(error) = first_admission_error {
                return Err(error);
            }
            if self.finished {
                return Ok(());
            }
            if self
                .video
                .iter()
                .take(bv)
                .chain(&self.audio)
                .all(|s| s.windows.is_empty())
            {
                return Ok(());
            }
        }
    }
    /// All encoder tails were drained; admitted windows remain consumable.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }
    fn fail(&mut self) {
        self.failed = true;
        for stage in self.video.iter_mut().chain(&mut self.audio) {
            stage.clear();
        }
        self.global_staged_packets = 0;
        self.global_staged_bytes = 0;
    }
    /// Run the bounded 2-second staging watchdog without requiring a new event.
    /// A stale dynamic (live) key fails alone; only a base stage fails the run.
    pub(crate) fn watchdog<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        if self.base_stale(now) {
            self.fail();
            delivery.abort();
            return Err(DispatchError::WatchdogExpired);
        }
        self.fail_stale_dynamic(now);
        self.reconcile();
        self.reconcile_audio();
        // Delivering post-fallback windows is not needed here; the next event
        // pumps them. Admission faults stay with the binding's own report.
        Ok(())
    }

    /// Append a dynamic (live) video stage. Never gates any other stage.
    ///
    /// # Errors
    /// `InvalidConfig` for a time base unequal to the cadence inverse, too many
    /// live dynamic keys, or a finished/failed dispatcher.
    pub(crate) fn add_video_key(
        &mut self,
        time_base: DtsTimeBase,
        gate: MoveGate,
    ) -> Result<usize, DispatchError> {
        let dynamic = self.video.len().saturating_sub(self.base_video);
        let live = self
            .video_meta
            .iter()
            .filter(|m| m.dynamic && !m.retired)
            .count();
        let reusable = self.reusable_slot();
        if self.failed
            || self.finished
            || live >= MAX_DYNAMIC_KEYS
            || (reusable.is_none() && dynamic >= MAX_DYNAMIC_SLOTS)
            || i128::from(time_base.num).checked_mul(i128::from(self.cadence.num))
                != i128::from(time_base.den).checked_mul(i128::from(self.cadence.den))
            || time_base.num <= 0
        {
            return Err(DispatchError::InvalidConfig);
        }
        let generation = self.next_key_generation;
        self.next_key_generation = self.next_key_generation.wrapping_add(1);
        if let Some(index) = reusable {
            // Fully reset: no staged window, watermark or DTS history of the
            // retired key survives. Its late events are the caller's to drop
            // (the router tags them with a per-worker generation).
            let (Some(stage), Some(tb), Some(meta)) = (
                self.video.get_mut(index),
                self.video_time_base.get_mut(index),
                self.video_meta.get_mut(index),
            ) else {
                return Err(DispatchError::InvalidConfig);
            };
            *stage = StreamStage::new();
            *tb = time_base;
            *meta = VideoMeta {
                dynamic: true,
                retired: false,
                failed: false,
                gate: Some(gate),
                params: None,
                generation,
            };
            return Ok(index);
        }
        let index = self.video.len();
        self.video.push(StreamStage::new());
        self.video_time_base.push(time_base);
        self.video_meta.push(VideoMeta {
            dynamic: true,
            retired: false,
            failed: false,
            gate: Some(gate),
            params: None,
            generation,
        });
        Ok(index)
    }
    /// A retired dynamic slot that no binding reads, moves to or falls back
    /// to (so no pending move or staged window can alias a new key there).
    fn reusable_slot(&self) -> Option<usize> {
        (self.base_video..self.video_meta.len()).find(|&k| {
            self.video_meta
                .get(k)
                .is_some_and(|m| m.dynamic && m.retired)
                && !self.bindings.iter().any(|b| {
                    b.video_key == Some(k)
                        || b.fallback == Some(k)
                        || b.mv.is_some_and(|m| m.key == k)
                })
        })
    }
    /// Retire a dynamic key; its staged windows are dropped and later events
    /// for it are counted, never an error.
    ///
    /// # Errors
    /// `InvalidConfig` for an unknown or base key.
    pub(crate) fn retire_video_key(&mut self, key: usize) -> Result<(), DispatchError> {
        let meta = self
            .video_meta
            .get_mut(key)
            .filter(|m| m.dynamic)
            .ok_or(DispatchError::InvalidConfig)?;
        meta.retired = true;
        // No pending move may outlive the key it names: the slot can be
        // reused by a later `add_video_key`, and a move left here would then
        // land on an unrelated key.
        for b in &mut self.bindings {
            if b.mv.is_some_and(|m| m.key == key) {
                b.mv = None;
                push_outcome(
                    &mut self.outcomes,
                    MoveOutcome::Refused {
                        output_index: b.output_index,
                        reason: MoveRefusal::EncoderNotReady,
                    },
                );
            }
        }
        if let Some(stage) = self.video.get_mut(key) {
            let dropped: usize = stage.windows.iter().map(|w| w.packets.len()).sum();
            stage.clear();
            self.counters.dropped_unbound = self
                .counters
                .dropped_unbound
                .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        }
        Ok(())
    }
    /// Register a video move for one output (latest wins). Resolved on the
    /// first window `>= at_tick` whose destination video passes the key's gate;
    /// refused when none of `max_windows` windows does.
    ///
    /// # Errors
    /// `InvalidConfig` for an unknown output, an audio-only output, or an
    /// unknown/retired/failed key.
    pub(crate) fn rebind_video(
        &mut self,
        output_index: usize,
        key: usize,
        at_tick: u64,
        max_windows: u32,
    ) -> Result<(), DispatchError> {
        let usable = self
            .video_meta
            .get(key)
            .is_some_and(|m| !m.failed && !m.retired);
        let has_gate = self.video_meta.get(key).is_some_and(|m| m.gate.is_some());
        let generation = self.video_meta.get(key).map_or(0, |m| m.generation);
        let Some(b) = self
            .bindings
            .iter_mut()
            .find(|b| b.output_index == output_index)
        else {
            return Err(DispatchError::InvalidConfig);
        };
        if b.video_key.is_none() || !usable {
            return Err(DispatchError::InvalidConfig);
        }
        if b.mv.take().is_some() {
            push_outcome(&mut self.outcomes, MoveOutcome::Superseded { output_index });
        }
        if b.video_key == Some(key) {
            push_outcome(
                &mut self.outcomes,
                MoveOutcome::Applied {
                    output_index,
                    key,
                    tick: at_tick,
                    parameter_sets_changed: false,
                },
            );
        } else if !has_gate {
            push_outcome(
                &mut self.outcomes,
                MoveOutcome::Refused {
                    output_index,
                    reason: MoveRefusal::NoGate,
                },
            );
        } else if max_windows == 0 {
            push_outcome(
                &mut self.outcomes,
                MoveOutcome::Refused {
                    output_index,
                    reason: MoveRefusal::EncoderNotReady,
                },
            );
        } else {
            b.mv = Some(PendingMove {
                key,
                generation,
                at_tick: i128::from(at_tick),
                max_windows,
                failed: 0,
            });
        }
        Ok(())
    }
    /// Install the parameter-set extractor of a base or live key (used to
    /// detect a decoder-configuration change at a move).
    ///
    /// # Errors
    /// `InvalidConfig` for an unknown key.
    pub(crate) fn set_parameter_sets(
        &mut self,
        key: usize,
        params: ParameterSets,
    ) -> Result<(), DispatchError> {
        let meta = self
            .video_meta
            .get_mut(key)
            .ok_or(DispatchError::InvalidConfig)?;
        meta.params = Some(params);
        Ok(())
    }
    /// Drain move/fallback outcomes (at most 64 are retained, oldest dropped).
    pub(crate) fn take_move_outcomes(&mut self) -> Vec<MoveOutcome> {
        self.outcomes.drain(..).collect()
    }
    /// The video key an output currently reads (test observation; the router
    /// tracks its own bindings from the move outcomes).
    #[cfg(test)]
    pub(crate) fn binding_video_key(&self, output_index: usize) -> Option<usize> {
        self.bindings
            .iter()
            .find(|b| b.output_index == output_index)
            .and_then(|b| b.video_key)
    }
    /// Append a dynamic per-output program-audio stage (ADR-0108 §5.2: an
    /// output's private encoded audio bus). Its packets carry the program
    /// stage's time base; it never gates a base window and fails alone.
    /// Returns `(key, generation)`; a pending move names both.
    ///
    /// # Errors
    /// `InvalidConfig` without a program-audio stage, too many live dynamic
    /// audio stages, or a finished/failed dispatcher.
    pub(crate) fn add_audio_key(&mut self) -> Result<(usize, u64), DispatchError> {
        let live = self.dyn_audio.iter().filter(|a| !a.retired).count();
        if self.failed || self.finished || self.audio.is_empty() || live >= MAX_DYNAMIC_KEYS {
            return Err(DispatchError::InvalidConfig);
        }
        let generation = self.next_key_generation;
        self.next_key_generation = self.next_key_generation.wrapping_add(1);
        let fresh = DynAudio {
            stage: StreamStage::new(),
            retired: false,
            failed: false,
            generation,
        };
        let reusable = (0..self.dyn_audio.len()).find(|&k| {
            self.dyn_audio.get(k).is_some_and(|a| a.retired)
                && !self
                    .bindings
                    .iter()
                    .any(|b| b.audio_key == Some(k) || b.audio_mv.is_some_and(|m| m.key == Some(k)))
        });
        if let Some(k) = reusable {
            if let Some(slot) = self.dyn_audio.get_mut(k) {
                *slot = fresh;
                return Ok((k, generation));
            }
        }
        if self.dyn_audio.len() >= MAX_DYNAMIC_SLOTS {
            return Err(DispatchError::InvalidConfig);
        }
        self.dyn_audio.push(fresh);
        Ok((self.dyn_audio.len() - 1, generation))
    }
    /// Retire a dynamic audio stage: staged windows dropped, later events
    /// counted, its subscribers return to the program stage.
    ///
    /// # Errors
    /// `InvalidConfig` for an unknown key.
    pub(crate) fn retire_audio_key(&mut self, key: usize) -> Result<(), DispatchError> {
        let a = self
            .dyn_audio
            .get_mut(key)
            .ok_or(DispatchError::InvalidConfig)?;
        a.retired = true;
        let dropped: usize = a.stage.windows.iter().map(|w| w.packets.len()).sum();
        a.stage.clear();
        self.counters.dropped_unbound = self
            .counters
            .dropped_unbound
            .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        self.reconcile_audio();
        Ok(())
    }
    /// Move one output's program-audio slot to `key` (`None` = the shared
    /// program stage) at packet DTS `at` (a global AAC grid boundary G):
    /// in the window holding G, packets with DTS < G come from the old
    /// source and packets from G from the new one. Latest wins.
    ///
    /// # Errors
    /// `InvalidConfig` for an unknown output, an output without program
    /// audio, or an unknown/retired/failed key.
    pub(crate) fn rebind_audio(
        &mut self,
        output_index: usize,
        key: Option<usize>,
        at: i64,
    ) -> Result<(), DispatchError> {
        let generation = key
            .and_then(|k| self.dyn_audio.get(k))
            .map_or(0, |a| a.generation);
        if !self.audio_usable(key, generation) || at < 0 {
            return Err(DispatchError::InvalidConfig);
        }
        let b = self
            .bindings
            .iter_mut()
            .find(|b| b.output_index == output_index && b.audio_tracks.contains(&0))
            .ok_or(DispatchError::InvalidConfig)?;
        if b.audio_mv.is_none() && b.audio_key == key {
            return Ok(());
        }
        b.audio_mv = Some(AudioMove {
            key,
            generation,
            at,
        });
        // A destination that never proves a window must not hold this
        // output forever: its staging watchdog starts now (it then fails
        // alone and the output stays on / returns to the program stage).
        if let Some(a) = key.and_then(|k| self.dyn_audio.get_mut(k)) {
            if a.stage.watermark.is_none() && !a.stage.finished {
                a.stage.first_unproved.get_or_insert_with(Instant::now);
            }
        }
        Ok(())
    }
    /// Stage one dynamic audio stage's emission (`finish` = its EOF tail),
    /// then commit every ready window exactly as [`Self::dispatch`] does. A
    /// fault in the stage fails that stage alone, never the run.
    ///
    /// # Errors
    /// Only what [`Self::dispatch`]'s drain returns for the base windows.
    pub(crate) fn dispatch_dynamic_audio<D: WindowDelivery>(
        &mut self,
        key: usize,
        packets: Vec<EncodedPacket>,
        watermark: Option<DtsWatermark>,
        finish: bool,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        if self.failed || self.finished {
            return Err(DispatchError::InvalidEvent);
        }
        let Some(tb) = self.audio_time_base.first().copied() else {
            return Err(DispatchError::InvalidConfig);
        };
        let cadence = self.cadence;
        let Some(a) = self.dyn_audio.get_mut(key) else {
            return Err(DispatchError::InvalidEvent);
        };
        if a.retired || a.failed {
            self.counters.dropped_unbound = self.counters.dropped_unbound.saturating_add(1);
        } else {
            let stage = &mut a.stage;
            let result = (|| -> Result<(), DispatchError> {
                if stage.finished {
                    return Err(DispatchError::InvalidEvent);
                }
                for p in packets {
                    if p.kind() != StreamKind::Audio {
                        return Err(DispatchError::InvalidEvent);
                    }
                    stage.put(p, tb, cadence, now)?;
                }
                if finish {
                    stage.finished = true;
                    stage.first_unproved = None;
                } else {
                    if watermark.is_none() {
                        stage.first_unproved.get_or_insert(now);
                    }
                    stage.advance(watermark)?;
                }
                Ok(())
            })();
            if result.is_err() {
                self.fail_dynamic_audio(key);
            }
        }
        match self.drain(now, delivery) {
            Ok(()) => Ok(()),
            Err(error @ DispatchError::Admission { .. }) => Err(error),
            Err(error) => {
                self.fail();
                delivery.abort();
                Err(error)
            }
        }
    }
    /// The dynamic audio stage an output's program slot reads (test
    /// observation).
    #[cfg(test)]
    pub(crate) fn binding_audio_key(&self, output_index: usize) -> Option<usize> {
        self.bindings
            .iter()
            .find(|b| b.output_index == output_index)
            .and_then(|b| b.audio_key)
    }
    /// True for an unknown, failed or retired dynamic audio stage.
    pub(crate) fn is_audio_key_failed(&self, key: usize) -> bool {
        self.dyn_audio
            .get(key)
            .is_none_or(|a| a.failed || a.retired)
    }
    pub(crate) fn video_key_count(&self) -> usize {
        self.video.len()
    }
    /// True for an unknown, failed or retired key.
    pub(crate) fn is_key_failed(&self, key: usize) -> bool {
        self.video_meta
            .get(key)
            .is_none_or(|m| m.failed || m.retired)
    }
    #[allow(dead_code)]
    // reason: read by the routing metrics of the HTTP API PR (ADR-0108 slice 1 follow-up); tested here.
    pub(crate) fn counters(&self) -> DispatchCounters {
        self.counters
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use ffmpeg_next::codec::packet::{Flags, Packet};
    use multiview_ffmpeg::{CodecKind, NalFraming};
    use multiview_output::batch::{BoundedBatchQueue, RecoveryGate};

    const TICK: i128 = 200_000_000;

    #[test]
    fn rational_dts_never_falls_into_the_prior_ntsc_window() {
        let frame_base = DtsTimeBase {
            num: 1001,
            den: 30_000,
        };
        for tick in 0..108_000_i64 {
            assert_eq!(
                frame_base.window_index(tick, Rational::FPS_29_97),
                Some(i128::from(tick))
            );
        }
        let samples = DtsTimeBase {
            num: 1,
            den: 48_000,
        };
        for sample in [0, 1024, 48_000, 480_000, 172_800_000] {
            assert_eq!(
                samples.window_index(sample, Rational::FPS_29_97),
                Some(i128::from(sample) * 30_000 / (48_000 * 1001))
            );
        }
        assert_eq!(frame_base.window_index(-1, Rational::FPS_29_97), Some(-1));
    }

    #[test]
    fn bounded_aac_negative_priming_joins_zero_window_without_timestamp_rewrite() {
        let mut stage = StreamStage::new();
        let now = Instant::now();
        let tb = DtsTimeBase {
            num: 1,
            den: 48_000,
        };
        stage
            .put(audio(-1024, 0), tb, Rational::FPS_29_97, now)
            .unwrap();
        assert_eq!(stage.windows.front().unwrap().index, 0);
        assert_eq!(stage.windows.front().unwrap().packets[0].dts(), Some(-1024));
        stage
            .put(audio(0, 0), tb, Rational::FPS_29_97, now)
            .unwrap();
        stage
            .put(audio(1024, 0), tb, Rational::FPS_29_97, now)
            .unwrap();
        assert_eq!(stage.take(0).len(), 3);
        assert!(stage.windows.is_empty());
        assert_eq!(
            stage.put(audio(-1024, 0), tb, Rational::FPS_29_97, now),
            Err(DispatchError::LatePacket)
        );
    }

    #[test]
    fn stale_window_is_rejected_instead_of_accumulating_to_overflow() {
        let mut stage = StreamStage::new();
        let now = Instant::now();
        let cadence = Rational::FPS_29_97;
        let tb = DtsTimeBase {
            num: 1001,
            den: 30_000,
        };
        stage.put(video(22, 1, true), tb, cadence, now).unwrap();
        assert_eq!(stage.windows.front().unwrap().index, 22);
        assert_eq!(
            stage.put(video(21, 1, false), tb, cadence, now),
            Err(DispatchError::LatePacket)
        );
    }

    fn packet(kind: StreamKind, dts: i64, len: usize, rap: bool) -> EncodedPacket {
        let mut raw = if rap {
            Packet::copy(&[0, 0, 0, 1, 0x65, 0x88])
        } else {
            Packet::new(len)
        };
        raw.set_pts(Some(dts));
        raw.set_dts(Some(dts));
        if rap {
            raw.set_flags(Flags::KEY);
        }
        EncodedPacket::with_kind(raw, kind)
    }
    fn video(dts: i64, len: usize, rap: bool) -> EncodedPacket {
        packet(StreamKind::Video, dts, len, rap)
    }
    fn audio(dts: i64, track: usize) -> EncodedPacket {
        packet(
            if track == 0 {
                StreamKind::Audio
            } else {
                StreamKind::AudioTrack(u16::try_from(track - 1).unwrap())
            },
            dts,
            8,
            false,
        )
    }
    struct FakeDelivery {
        queues: Vec<BoundedBatchQueue>,
        windows: Vec<(usize, usize)>,
        aborted: bool,
    }
    impl FakeDelivery {
        fn new(count: usize) -> Self {
            Self {
                queues: (0..count)
                    .map(|_| {
                        BoundedBatchQueue::new().with_recovery_gate(RecoveryGate::StrictRap {
                            codec: CodecKind::H264,
                            framing: NalFraming::AnnexB,
                        })
                    })
                    .collect(),
                windows: Vec::new(),
                aborted: false,
            }
        }
    }
    impl WindowDelivery for FakeDelivery {
        fn offer_window(
            &mut self,
            output_index: usize,
            _generation: u64,
            packets: Vec<EncodedPacket>,
            span: Duration,
            now: Instant,
        ) -> Result<(), multiview_output::batch::EmissionError> {
            let count = packets.len();
            self.windows.push((output_index, count));
            self.queues[output_index].offer_emitted(packets, span, now)
        }
        fn report_loss(
            &mut self,
            output_index: usize,
            reason: multiview_output::batch::OfferError,
        ) {
            self.queues[output_index].report_loss(reason);
        }
        fn mark_discontinuity(
            &mut self,
            output_index: usize,
            reason: multiview_output::batch::OfferError,
        ) {
            self.queues[output_index].mark_discontinuity(reason);
        }
        fn take_discontinuity(
            &mut self,
            output_index: usize,
        ) -> Option<multiview_output::batch::Discontinuity> {
            self.queues[output_index].take_discontinuity()
        }
        fn generation(&self, output_index: usize) -> u64 {
            self.queues[output_index].generation()
        }
        fn statuses(&self) -> Vec<crate::binding_delivery::BindingStatus> {
            Vec::new()
        }
        fn finish(&mut self) {}
        fn abort(&mut self) {
            self.aborted = true;
            for q in &mut self.queues {
                q.report_loss(multiview_output::batch::OfferError::ProducerLoss);
                q.close();
            }
        }
    }
    fn cfg(bindings: Vec<BindingConfig>, video: usize, audio: usize) -> DispatchConfig {
        DispatchConfig {
            video_count: video,
            audio_count: audio,
            video_time_base: vec![DtsTimeBase { num: 1, den: 5 }; video],
            audio_time_base: vec![DtsTimeBase { num: 1, den: 5 }; audio],
            bindings,
            first_tick: 0,
            cadence: Rational::new(5, 1),
            video_move_gates: Vec::new(),
        }
    }
    fn binding(i: usize, v: usize, a: usize) -> BindingConfig {
        BindingConfig {
            output_index: i,
            video_key: Some(v),
            audio_tracks: (0..a).collect(),
        }
    }
    fn mark(ns: i128) -> DtsWatermark {
        DtsWatermark {
            complete_before_ns: ns,
        }
    }
    fn ve(
        key_index: usize,
        tick: u64,
        packets: Vec<EncodedPacket>,
        watermark: Option<DtsWatermark>,
    ) -> DispatchEvent {
        DispatchEvent::Video {
            key_index,
            tick,
            timebase: Rational::new(1, 5),
            packets,
            watermark,
        }
    }
    fn ae(
        tick: u64,
        packets: Vec<EncodedPacket>,
        watermarks: Vec<Option<DtsWatermark>>,
    ) -> DispatchEvent {
        DispatchEvent::Audio {
            tick,
            timebases: vec![Rational::new(1, 5); watermarks.len()],
            packets,
            watermarks,
        }
    }
    #[test]
    fn video_encoder_timebase_must_equal_inverse_program_cadence() {
        let mut config = cfg(vec![binding(0, 0, 1)], 1, 1);
        config.video_time_base[0] = DtsTimeBase { num: 1, den: 30 };
        assert!(matches!(
            RenditionDispatcher::new(config),
            Err(DispatchError::InvalidConfig)
        ));
    }
    #[test]
    fn zero_then_many_delayed_waits_for_both_proven_watermarks() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        let mut sink = FakeDelivery::new(1);
        d.dispatch(ve(0, 0, vec![], None), now, &mut sink).unwrap();
        d.dispatch(
            ae(0, vec![audio(0, 0)], vec![Some(mark(TICK))]),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.windows.len(), 0);
        d.dispatch(
            ve(
                0,
                1,
                (0..5).map(|_| video(0, 6, false)).collect(),
                Some(mark(TICK - 1)),
            ),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.windows.len(), 0);
        d.dispatch(ve(0, 2, vec![], Some(mark(TICK))), now, &mut sink)
            .unwrap();
        assert_eq!(sink.windows, vec![(0, 6)]);
        assert_eq!(
            sink.queues[0].pop_at(now).unwrap().unwrap().packets().len(),
            6
        );
    }
    #[test]
    fn distinct_audio_track_bases_are_checked_per_stream_not_inherited() {
        let now = Instant::now();
        let mut config = cfg(vec![binding(0, 0, 2)], 1, 2);
        config.audio_time_base[1] = DtsTimeBase { num: 1, den: 10 };
        let mut d = RenditionDispatcher::new(config).unwrap();
        let mut sink = FakeDelivery::new(1);
        d.dispatch(
            ve(0, 0, vec![video(0, 8, true)], Some(mark(TICK))),
            now,
            &mut sink,
        )
        .unwrap();
        let event = DispatchEvent::Audio {
            tick: 0,
            timebases: vec![Rational::new(1, 5), Rational::new(1, 10)],
            packets: vec![audio(0, 0), audio(0, 1)],
            watermarks: vec![Some(mark(TICK)), None],
        };
        d.dispatch(event, now, &mut sink).unwrap();
        assert_eq!(sink.windows.len(), 0); // unproven discrete track gates BOTH streams
        let event = DispatchEvent::Audio {
            tick: 1,
            timebases: vec![Rational::new(1, 5), Rational::new(1, 10)],
            packets: vec![],
            watermarks: vec![Some(mark(TICK)), Some(mark(TICK))],
        };
        d.dispatch(event, now, &mut sink).unwrap();
        assert_eq!(sink.windows, vec![(0, 3)]);
        let lease = sink.queues[0].pop_at(now).unwrap().unwrap();
        assert_eq!(lease.packets()[2].kind(), StreamKind::AudioTrack(0));
    }
    #[test]
    fn two_video_three_bindings_one_shared_audio_and_slow_peer() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(cfg(
            vec![binding(0, 0, 1), binding(1, 1, 1), binding(2, 1, 1)],
            2,
            1,
        ))
        .unwrap();
        let mut sink = FakeDelivery::new(3);
        d.dispatch(
            ve(0, 0, vec![video(0, 8, true)], Some(mark(TICK))),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            ve(1, 0, vec![video(0, 8, true)], Some(mark(TICK))),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            ae(0, vec![audio(0, 0)], vec![Some(mark(TICK))]),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.windows, vec![(0, 2), (1, 2), (2, 2)]);
        let held = sink.queues[1].pop_at(now).unwrap().unwrap();
        sink.queues[1] = BoundedBatchQueue::with_limits(1, 1, Duration::from_secs(2))
            .with_recovery_gate(RecoveryGate::StrictRap {
                codec: CodecKind::H264,
                framing: NalFraming::AnnexB,
            });
        d.dispatch(
            ve(0, 1, vec![video(1, 8, false)], Some(mark(TICK * 2))),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            ve(1, 1, vec![video(1, 8, false)], Some(mark(TICK * 2))),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            d.dispatch(
                ae(1, vec![audio(1, 0)], vec![Some(mark(TICK * 2))]),
                now,
                &mut sink
            ),
            Err(DispatchError::Admission { output_index: 1 })
        );
        assert_eq!(sink.queues[0].queued_windows(), 2);
        assert_eq!(sink.queues[2].queued_windows(), 2);
        assert!(!sink.aborted);
        assert!(held.is_current());
    }
    #[test]
    fn five_aac_whole_reject_without_partial_video_or_audio() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(cfg(vec![binding(0, 0, 5)], 1, 5)).unwrap();
        let mut sink = FakeDelivery::new(1);
        sink.queues[0] = BoundedBatchQueue::with_limits(4, 100, Duration::from_secs(2))
            .with_recovery_gate(RecoveryGate::StrictRap {
                codec: CodecKind::H264,
                framing: NalFraming::AnnexB,
            });
        d.dispatch(
            ve(0, 0, vec![video(0, 8, true)], Some(mark(TICK))),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            d.dispatch(
                ae(
                    0,
                    (0..5).map(|i| audio(0, i)).collect(),
                    vec![Some(mark(TICK)); 5]
                ),
                now,
                &mut sink
            ),
            Err(DispatchError::Admission { output_index: 0 })
        );
        assert_eq!(sink.queues[0].reserved(), (0, 0));
        assert_eq!(sink.windows, vec![(0, 6)]);
    }
    #[test]
    fn eleven_megabyte_packet_retains_lease_budget_and_payload_pointer() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(cfg(
            vec![binding(0, 0, 0), binding(1, 0, 0), binding(2, 0, 0)],
            1,
            0,
        ))
        .unwrap();
        let mut sink = FakeDelivery::new(3);
        d.dispatch(
            ve(
                0,
                0,
                vec![video(0, 11 * 1024 * 1024, false)],
                Some(mark(TICK)),
            ),
            now,
            &mut sink,
        )
        .unwrap();
        let a = sink.queues[0].pop_at(now).unwrap().unwrap();
        let b = sink.queues[1].pop_at(now).unwrap().unwrap();
        let c = sink.queues[2].pop_at(now).unwrap().unwrap();
        assert_eq!(
            a.packets()[0].payload().unwrap().as_ptr(),
            b.packets()[0].payload().unwrap().as_ptr()
        );
        assert_eq!(
            b.packets()[0].payload().unwrap().as_ptr(),
            c.packets()[0].payload().unwrap().as_ptr()
        );
        assert_eq!(sink.queues[0].reserved().1, 11 * 1024 * 1024);
        drop(a);
        assert_eq!(sink.queues[0].reserved(), (0, 0));
        drop(b);
        drop(c);
    }
    #[test]
    fn eof_tail_remains_drained_and_missing_proof_triggers_bounded_abort() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        let mut sink = FakeDelivery::new(1);
        d.dispatch(ve(0, 0, vec![], None), now, &mut sink).unwrap();
        d.dispatch(ae(0, vec![], vec![None]), now, &mut sink)
            .unwrap();
        assert_eq!(
            d.watchdog(
                now + Duration::from_secs(2) + Duration::from_nanos(1),
                &mut sink
            ),
            Err(DispatchError::WatchdogExpired)
        );
        assert!(sink.aborted);
        let mut d = RenditionDispatcher::new(cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        let mut sink = FakeDelivery::new(1);
        d.dispatch(ve(0, 0, vec![], None), now, &mut sink).unwrap();
        d.dispatch(ae(0, vec![], vec![None]), now, &mut sink)
            .unwrap();
        d.dispatch(
            DispatchEvent::FinishVideo {
                key_index: 0,
                packets: vec![video(0, 8, true)],
            },
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            DispatchEvent::FinishAudio {
                packets: vec![audio(0, 0)],
            },
            now,
            &mut sink,
        )
        .unwrap();
        assert!(d.is_finished());
        let held = sink.queues[0].pop_at(now).unwrap().unwrap();
        assert_eq!(held.packets().len(), 2);
        assert!(held.is_current());
        drop(held);
    }
    #[test]
    fn real_binding_delivery_accepts_one_joined_window_and_retains_shared_payload() {
        use crate::binding_delivery::{BindingDelivery, BindingPolicy, DeliveryLimits};
        let now = Instant::now();
        let gate = RecoveryGate::StrictRap {
            codec: CodecKind::H264,
            framing: NalFraming::AnnexB,
        };
        let (mut delivery, receivers) = BindingDelivery::new(
            vec![
                BindingPolicy::Bounded,
                BindingPolicy::Bounded,
                BindingPolicy::Bounded,
            ],
            DeliveryLimits::default(),
            vec![Some(gate); 3],
        );
        let mut d = RenditionDispatcher::new(cfg(
            vec![binding(0, 0, 1), binding(1, 0, 1), binding(2, 0, 1)],
            1,
            1,
        ))
        .unwrap();
        d.dispatch(
            ve(0, 0, vec![video(0, 6, true)], Some(mark(TICK))),
            now,
            &mut delivery,
        )
        .unwrap();
        d.dispatch(
            ae(0, vec![audio(0, 0)], vec![Some(mark(TICK))]),
            now,
            &mut delivery,
        )
        .unwrap();
        let mut pointers = Vec::new();
        for rx in &receivers {
            let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let second = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(first.packet.kind(), StreamKind::Video);
            assert_eq!(second.packet.kind(), StreamKind::Audio);
            pointers.push(first.packet.payload().unwrap().as_ptr());
        }
        assert_eq!(pointers[0], pointers[1]);
        assert_eq!(pointers[1], pointers[2]);
        let statuses = delivery.statuses();
        assert_eq!(statuses.len(), 3);
        assert!(statuses
            .iter()
            .all(|s| s.admitted_batches == 1 && s.rejected_batches == 0));
        delivery.finish();
    }
    #[test]
    fn exact_2997_hour_boundary_avoids_fixed_period_drift() {
        let cadence = Rational::FPS_29_97;
        let tick = 107_892_i128;
        let exact = RenditionDispatcher::tick_boundary(tick, cadence).unwrap();
        assert_eq!(exact, tick * 1001 * 1_000_000_000 / 30_000);
        assert_ne!(
            exact,
            tick * RenditionDispatcher::tick_boundary(1, cadence).unwrap()
        );
    }

    // ---- live keys / rebind ----
    fn rap77(dts: i64) -> EncodedPacket {
        let mut bytes = vec![0_u8; 77];
        bytes[..6].copy_from_slice(&[0, 0, 0, 1, 0x65, 0x88]);
        let mut raw = Packet::copy(&bytes);
        raw.set_pts(Some(dts));
        raw.set_dts(Some(dts));
        raw.set_flags(Flags::KEY);
        EncodedPacket::with_kind(raw, StreamKind::Video)
    }
    fn gate() -> MoveGate {
        Box::new(|p| p.is_keyframe() && p.len() == 77)
    }
    fn gated_cfg(bindings: Vec<BindingConfig>, video: usize, audio: usize) -> DispatchConfig {
        let mut c = cfg(bindings, video, audio);
        c.video_move_gates = (0..video).map(|_| Some(gate())).collect();
        c
    }
    /// Feed base window `i` (all base keys + one audio) at `now`.
    fn feed_base(
        d: &mut RenditionDispatcher,
        sink: &mut FakeDelivery,
        keys: usize,
        i: i64,
        now: Instant,
        first: impl Fn(usize) -> EncodedPacket,
    ) -> Result<(), DispatchError> {
        let m = Some(mark(TICK * i128::from(i + 1)));
        let tick = u64::try_from(i).unwrap();
        for k in 0..keys {
            d.dispatch(ve(k, tick, vec![first(k)], m), now, sink)?;
        }
        d.dispatch(ae(tick, vec![audio(i, 0)], vec![m]), now, sink)
    }
    fn plain(i: i64) -> impl Fn(usize) -> EncodedPacket {
        move |_| video(i, 8, i == 0)
    }
    /// Pop and release every queued window of one output: (first len, count).
    fn pop_all(sink: &mut FakeDelivery, o: usize, now: Instant) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        while let Some(Some(lease)) = sink.queues[o].pop_at(now).ok().map(Some) {
            let Some(l) = lease else { break };
            out.push((l.packets()[0].len(), l.packets().len()));
        }
        out
    }

    #[test]
    fn default_topology_delivers_exactly_as_before() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        let mut sink = FakeDelivery::new(1);
        for i in 0..3 {
            feed_base(&mut d, &mut sink, 1, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
        }
        assert_eq!(sink.windows, vec![(0, 2), (0, 2), (0, 2)]);
        assert_eq!(d.counters(), DispatchCounters::default());
        assert_eq!(d.video_key_count(), 1);
        assert_eq!(d.binding_video_key(0), Some(0));
        assert_eq!(d.take_move_outcomes(), vec![]);
    }

    #[test]
    fn a_slow_live_key_never_delays_base_windows() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        assert_eq!(key, 2);
        for i in 0..10_i64 {
            if i >= 6 {
                let m = Some(mark(TICK * i128::from(i + 1)));
                d.dispatch(
                    ve(2, u64::try_from(i).unwrap(), vec![rap77(i)], m),
                    now,
                    &mut sink,
                )
                .unwrap();
            }
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            let n = usize::try_from(i + 1).unwrap();
            // Exactly on closure of window i, both outputs got window i.
            assert_eq!(sink.windows.len(), 2 * n, "window {i} delayed or gapped");
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
        }
        assert_eq!(d.counters().dynamic_failed, 0);
    }

    #[test]
    fn a_wedged_live_key_never_delays_base_windows_and_fails_alone() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.rebind_video(1, key, 2, 3).unwrap();
        for i in 0..6_i64 {
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            assert_eq!(sink.windows.len(), 2 * usize::try_from(i + 1).unwrap());
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
            if i < 4 {
                assert!(
                    d.take_move_outcomes().is_empty(),
                    "refused too early at {i}"
                );
            }
        }
        // Windows 2,3,4 failed the gate: refused, B kept key 1.
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Refused {
                output_index: 1,
                reason: MoveRefusal::EncoderNotReady
            }]
        );
        assert_eq!(d.binding_video_key(1), Some(1));
        // The wedged key goes stale: it fails alone, the run survives.
        let t0 = now;
        d.dispatch(ve(2, 0, vec![], None), t0, &mut sink).unwrap();
        let later = now + Duration::from_secs(3);
        d.watchdog(later, &mut sink).unwrap();
        assert!(d.is_key_failed(2));
        assert_eq!(d.counters().dynamic_failed, 1);
        assert!(!sink.aborted);
        feed_base(&mut d, &mut sink, 2, 6, later, plain(6)).unwrap();
        assert_eq!(sink.windows.len(), 14);
    }

    #[test]
    fn move_lands_on_first_window_with_rap_and_parameter_sets() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.rebind_video(1, key, 2, 10).unwrap();
        let mut b_windows = Vec::new();
        for i in 0..6_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let pkt = match i {
                3 => video(i, 6, true), // IDR without parameter sets (len != 77)
                4 => rap77(i),
                _ => video(i, 8, false),
            };
            d.dispatch(
                ve(2, u64::try_from(i).unwrap(), vec![pkt], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            b_windows.extend(pop_all(&mut sink, 1, now));
        }
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Applied {
                output_index: 1,
                key: 2,
                tick: 4,
                parameter_sets_changed: false
            }]
        );
        assert_eq!(d.binding_video_key(1), Some(2));
        let firsts: Vec<usize> = b_windows.iter().map(|w| w.0).collect();
        // windows 0..=3 carry key 1 (len 8, or len 6 for a RAP-less IDR never used), 4 the gated RAP.
        assert_eq!(firsts.len(), 6);
        assert!(firsts[..4].iter().all(|&l| l == 8 || l == 6));
        assert_eq!(firsts[4], 77);
        assert_eq!(firsts[5], 8);
    }

    /// E (lens 1 #2): the destination window's SECOND video packet is the
    /// gated RAP. The move applies on that window and the delivered window
    /// starts at the RAP (the leading non-RAP is dropped, audio kept).
    #[test]
    fn move_gate_passes_on_a_later_rap_in_the_window_and_cuts_there() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.rebind_video(1, key, 2, 1).unwrap();
        let mut b_windows = Vec::new();
        for i in 0..4_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let pkts = if i == 2 {
                vec![video(i, 8, false), rap77(i)]
            } else {
                vec![video(i, 8, false)]
            };
            d.dispatch(ve(key, u64::try_from(i).unwrap(), pkts, m), now, &mut sink)
                .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            b_windows.extend(pop_all(&mut sink, 1, now));
        }
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Applied {
                output_index: 1,
                key,
                tick: 2,
                parameter_sets_changed: false
            }]
        );
        assert_eq!(b_windows.len(), 4);
        assert_eq!(b_windows[2], (77, 2), "cut at the RAP, audio kept");
        assert_eq!(b_windows[3], (8, 2));
    }

    /// F (lens 1 #3): run a move onto a live key whose RAP's parameter sets
    /// `differ` (or not) from the base key's last delivered RAP. Returns the
    /// outcome and the number of discontinuities the moved output saw.
    fn move_with_sets(differ: bool) -> (Vec<MoveOutcome>, usize) {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        // Synthetic extractor: a keyframe's "sets" are its length (base RAPs
        // are 8 bytes, the live RAP 77) unless the sets are declared equal.
        for k in [1, key] {
            d.set_parameter_sets(
                k,
                Box::new(move |p: &EncodedPacket| {
                    if !p.is_keyframe() {
                        RapParameterSets::Absent
                    } else if differ {
                        RapParameterSets::Sets(vec![u8::try_from(p.len()).unwrap()])
                    } else {
                        RapParameterSets::Sets(vec![1])
                    }
                }),
            )
            .unwrap();
        }
        d.rebind_video(1, key, 2, 3).unwrap();
        let mut discontinuities = 0;
        for i in 0..5_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let pkt = if i == 2 { rap77(i) } else { video(i, 8, false) };
            d.dispatch(
                ve(key, u64::try_from(i).unwrap(), vec![pkt], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
            if sink.queues[1].take_discontinuity().is_some() {
                discontinuities += 1;
            }
        }
        assert_eq!(d.binding_video_key(1), Some(key));
        assert_eq!(
            sink.windows.iter().filter(|w| w.0 == 1).count(),
            5,
            "no window lost"
        );
        (d.take_move_outcomes(), discontinuities)
    }

    #[test]
    fn a_move_with_changed_parameter_sets_reports_one_discontinuity() {
        let (outcomes, discontinuities) = move_with_sets(true);
        assert_eq!(
            outcomes,
            vec![MoveOutcome::Applied {
                output_index: 1,
                key: 2,
                tick: 2,
                parameter_sets_changed: true
            }]
        );
        assert_eq!(discontinuities, 1, "exactly once, before the cut window");
        let (outcomes, discontinuities) = move_with_sets(false);
        assert_eq!(
            outcomes,
            vec![MoveOutcome::Applied {
                output_index: 1,
                key: 2,
                tick: 2,
                parameter_sets_changed: false
            }]
        );
        assert_eq!(discontinuities, 0, "equal sets splice");
    }

    /// Extractor whose keyframe sets are `sets` (any non-keyframe: Absent).
    fn fixed_sets(sets: RapParameterSets) -> ParameterSets {
        Box::new(move |p: &EncodedPacket| {
            if p.is_keyframe() {
                sets.clone()
            } else {
                RapParameterSets::Absent
            }
        })
    }

    /// M (lens 1 r2 #3): a cut RAP whose parameter-set scan was Rejected
    /// (malformed) is a change — one discontinuity — never a silent splice.
    #[test]
    fn a_rejected_parameter_set_scan_at_a_move_is_a_change() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.set_parameter_sets(1, fixed_sets(RapParameterSets::Sets(vec![1])))
            .unwrap();
        d.set_parameter_sets(key, fixed_sets(RapParameterSets::Rejected))
            .unwrap();
        d.rebind_video(1, key, 2, 3).unwrap();
        let mut discontinuities = 0;
        for i in 0..4_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let pkt = if i == 2 { rap77(i) } else { video(i, 8, false) };
            d.dispatch(
                ve(key, u64::try_from(i).unwrap(), vec![pkt], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
            if sink.queues[1].take_discontinuity().is_some() {
                discontinuities += 1;
            }
        }
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Applied {
                output_index: 1,
                key,
                tick: 2,
                parameter_sets_changed: true
            }]
        );
        assert_eq!(discontinuities, 1);
    }

    /// L (lens 1 r2 #2): a FALLBACK onto a key whose sets differ from the
    /// ones the output carried is decided once at the GOP-opening packet:
    /// exactly one discontinuity, and the window is still admitted.
    #[test]
    fn a_fallback_with_changed_parameter_sets_reports_one_discontinuity() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.set_parameter_sets(1, fixed_sets(RapParameterSets::Sets(vec![1])))
            .unwrap();
        d.set_parameter_sets(key, fixed_sets(RapParameterSets::Sets(vec![2])))
            .unwrap();
        d.rebind_video(1, key, 0, 10).unwrap();
        for i in 0..3_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let p = if i == 0 { rap77(i) } else { video(i, 8, false) };
            d.dispatch(
                ve(key, u64::try_from(i).unwrap(), vec![p], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
        }
        assert_eq!(d.binding_video_key(1), Some(key));
        let _ = sink.queues[1].take_discontinuity();
        let _ = d.take_move_outcomes();
        // The live key is closed (retired); window 3's base video for key 1
        // is a RAP, so the fallback loses no window (no loss signal that
        // could mask a missing decision).
        d.retire_video_key(key).unwrap();
        let rap_on_1 = |i: i64| {
            move |k: usize| {
                if k == 1 {
                    rap77(i)
                } else {
                    video(i, 8, false)
                }
            }
        };
        feed_base(&mut d, &mut sink, 2, 3, now, rap_on_1(3)).unwrap();
        pop_all(&mut sink, 0, now);
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Fallback { output_index: 1 }]
        );
        assert_eq!(pop_all(&mut sink, 1, now), vec![(77, 2)], "admitted");
        let event = sink.queues[1].take_discontinuity();
        assert_eq!(
            event.map(|e| e.reason),
            Some(OfferError::DecoderConfigChanged),
            "the fallback reset the mux, without a loss"
        );
        assert_eq!(sink.queues[1].take_discontinuity(), None, "exactly one");
        // Steady state afterwards: the same sets on key 1 never reset again.
        feed_base(&mut d, &mut sink, 2, 4, now, rap_on_1(4)).unwrap();
        pop_all(&mut sink, 0, now);
        pop_all(&mut sink, 1, now);
        assert_eq!(sink.queues[1].take_discontinuity(), None);
    }

    #[test]
    fn unbound_and_retired_key_events_are_dropped_and_counted_never_abort() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        let mut sink = FakeDelivery::new(1);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        // Registered but unbound: staged, pruned once nobody can need it.
        d.dispatch(
            ve(key, 0, vec![video(0, 8, false)], Some(mark(TICK))),
            now,
            &mut sink,
        )
        .unwrap();
        feed_base(&mut d, &mut sink, 1, 0, now, plain(0)).unwrap();
        assert_eq!(d.counters().dropped_late_dynamic, 1);
        assert!(d.retire_video_key(0).is_err()); // base keys cannot retire
        d.retire_video_key(key).unwrap();
        d.dispatch(
            ve(key, 1, vec![video(1, 8, false)], Some(mark(TICK * 2))),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            DispatchEvent::FinishVideo {
                key_index: key,
                packets: vec![],
            },
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(d.counters().dropped_unbound, 2);
        assert!(d.is_key_failed(key));
        assert!(d.rebind_video(0, key, 0, 3).is_err());
        assert!(!sink.aborted);
        // A key never registered is still an invalid event.
        assert_eq!(
            d.dispatch(ve(9, 0, vec![], None), now, &mut sink),
            Err(DispatchError::InvalidEvent)
        );
    }

    /// A real H.264 Annex B access unit: SPS (with `sps_tag` in its payload)
    /// + PPS + IDR slice when `idr`, else a single non-IDR slice.
    fn h264(dts: i64, idr: bool, sps_tag: u8) -> EncodedPacket {
        let bytes: Vec<u8> = if idr {
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, sps_tag, 0xDA, //
                0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80, //
                0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33,
            ]
        } else {
            vec![0, 0, 0, 1, 0x41, 0x9A, 0x02, 0x04, 0x11]
        };
        let mut raw = Packet::copy(&bytes);
        raw.set_pts(Some(dts));
        raw.set_dts(Some(dts));
        if idr {
            raw.set_flags(Flags::KEY);
        }
        EncodedPacket::with_kind(raw, StreamKind::Video)
    }
    fn h264_gate() -> MoveGate {
        Box::new(|p| {
            p.payload().is_some_and(|au| {
                multiview_ffmpeg::idr::is_self_contained_idr(
                    au,
                    CodecKind::H264,
                    NalFraming::AnnexB,
                )
            })
        })
    }
    fn h264_sets() -> ParameterSets {
        Box::new(|p| {
            if !p.is_keyframe() {
                return RapParameterSets::Absent;
            }
            let Some(au) = p.payload() else {
                return RapParameterSets::Absent;
            };
            match multiview_ffmpeg::idr::scan_parameter_sets(
                au,
                CodecKind::H264,
                NalFraming::AnnexB,
            ) {
                multiview_ffmpeg::idr::ParameterSetScan::Absent => RapParameterSets::Absent,
                multiview_ffmpeg::idr::ParameterSetScan::Rejected => RapParameterSets::Rejected,
                multiview_ffmpeg::idr::ParameterSetScan::Found(sets) => {
                    RapParameterSets::Sets(sets.concat())
                }
            }
        })
    }

    /// K (lens 1 r2 #1): a move whose cut RAP carries different parameter
    /// sets, on the REAL `BindingDelivery` with a strict H.264 RAP gate. The
    /// windows before the cut are still queued (the sink has not pulled), the
    /// cut window `[SPS+PPS+IDR, audio]` is admitted, nothing queued before it
    /// is flushed, and exactly one discontinuity (a new generation starting at
    /// the cut window) is reported. Fails with a loss-based signal: the
    /// queued backlog is flushed and counted as a rejection.
    #[test]
    fn a_parameter_set_change_at_a_move_admits_the_cut_window_and_flushes_nothing() {
        const CUT: i64 = 5;
        use crate::binding_delivery::{BindingDelivery, BindingPolicy, DeliveryLimits};
        let now = Instant::now();
        let gate = RecoveryGate::StrictRap {
            codec: CodecKind::H264,
            framing: NalFraming::AnnexB,
        };
        let (mut delivery, mut receivers) = BindingDelivery::new(
            vec![BindingPolicy::Bounded],
            DeliveryLimits::default(),
            vec![Some(gate)],
        );
        // The sink (mux) side: pulls every packet until the run ends.
        let rx = receivers.remove(0);
        let sink = std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Ok(p) = rx.recv() {
                got.push((p.packet.dts(), p.packet.kind(), p.permit.generation()));
            }
            got
        });
        let mut c = cfg(vec![binding(0, 0, 1)], 1, 1);
        c.video_move_gates = vec![Some(h264_gate())];
        let mut d = RenditionDispatcher::new(c).unwrap();
        d.set_parameter_sets(0, h264_sets()).unwrap();
        let live = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, h264_gate())
            .unwrap();
        d.set_parameter_sets(live, h264_sets()).unwrap();
        d.rebind_video(0, live, u64::try_from(CUT).unwrap(), 3)
            .unwrap();
        for i in 0..8_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let tick = u64::try_from(i).unwrap();
            d.dispatch(
                ve(live, tick, vec![h264(i, i == CUT, 0xBB)], m),
                now,
                &mut delivery,
            )
            .unwrap();
            d.dispatch(
                ve(0, tick, vec![h264(i, i == 0, 0xAA)], m),
                now,
                &mut delivery,
            )
            .unwrap();
            d.dispatch(ae(tick, vec![audio(i, 0)], vec![m]), now, &mut delivery)
                .unwrap();
        }
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Applied {
                output_index: 0,
                key: live,
                tick: 5,
                parameter_sets_changed: true
            }]
        );
        let discontinuity = delivery.take_discontinuity(0).expect("one discontinuity");
        assert_eq!(discontinuity.flushed_windows, 0, "nothing flushed");
        assert_eq!(delivery.take_discontinuity(0), None, "exactly one");
        let status = &delivery.statuses()[0];
        assert_eq!(status.admitted_batches, 8);
        assert_eq!(status.rejected_batches, 0);
        delivery.finish();
        // Sink side: every window arrived (2 packets each); the cut window
        // opens the new generation with the IDR first, audio after it.
        let got = sink.join().unwrap();
        let gens: Vec<u64> = got.iter().map(|g| g.2).collect();
        assert_eq!(gens, [[0; 10].as_slice(), [1; 6].as_slice()].concat());
        assert_eq!(got[10].0, Some(CUT));
        assert_eq!(got[10].1, StreamKind::Video);
        assert_eq!(got[11].1, StreamKind::Audio);
    }

    /// J (lens 2 r2 #1): output A has a pending move to live key K; K is
    /// closed (the router's linger/close path retires it) and, on a later
    /// tick, its slot is reused by a new key for output B. A is refused and
    /// keeps its key; a RAP on the reused slot never lands A's old move.
    #[test]
    fn a_move_to_a_retired_key_is_refused_and_never_lands_on_its_reused_slot() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let k = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        // A = output 0, moving to K from tick 1 for up to 10 windows.
        d.rebind_video(0, k, 1, 10).unwrap();
        feed_base(&mut d, &mut sink, 2, 0, now, plain(0)).unwrap();
        pop_all(&mut sink, 0, now);
        pop_all(&mut sink, 1, now);
        d.retire_video_key(k).unwrap();
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Refused {
                output_index: 0,
                reason: MoveRefusal::EncoderNotReady
            }],
            "retiring K refuses every move naming it at once"
        );
        // Later tick: B's key takes the freed slot (same index).
        feed_base(&mut d, &mut sink, 2, 1, now, plain(1)).unwrap();
        let reused = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        assert_eq!(reused, k, "the retired slot is reused");
        d.rebind_video(1, reused, 2, 10).unwrap();
        for i in 2..5_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            d.dispatch(
                ve(reused, u64::try_from(i).unwrap(), vec![rap77(i)], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
        }
        assert_eq!(d.binding_video_key(0), Some(0), "A's key unchanged");
        assert_eq!(d.binding_video_key(1), Some(reused), "B moved");
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Applied {
                output_index: 1,
                key: reused,
                tick: 2,
                parameter_sets_changed: false
            }],
            "no Applied for A"
        );
    }

    /// J: a pending move carries its key's generation; a move that somehow
    /// survived a slot reuse (stale bookkeeping) is refused, never applied.
    #[test]
    fn a_stale_generation_move_is_refused_on_a_reused_slot() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let k = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.rebind_video(0, k, 1, 10).unwrap();
        let stale = d.bindings[0].mv;
        d.retire_video_key(k).unwrap();
        let _ = d.take_move_outcomes();
        let reused = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        assert_eq!(reused, k);
        // Re-inject the old move (as if its clear had been missed).
        d.bindings[0].mv = stale;
        for i in 0..3_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            d.dispatch(
                ve(reused, u64::try_from(i).unwrap(), vec![rap77(i)], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            pop_all(&mut sink, 1, now);
        }
        assert_eq!(d.binding_video_key(0), Some(0));
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Refused {
                output_index: 0,
                reason: MoveRefusal::EncoderNotReady
            }]
        );
    }

    #[test]
    fn add_key_refuses_a_time_base_unequal_to_the_cadence() {
        let mut d = RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        assert_eq!(
            d.add_video_key(DtsTimeBase { num: 1, den: 30 }, gate()),
            Err(DispatchError::InvalidConfig)
        );
        assert_eq!(d.video_key_count(), 1);
        for _ in 0..MAX_DYNAMIC_KEYS {
            d.add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
                .unwrap();
        }
        assert_eq!(
            d.add_video_key(DtsTimeBase { num: 1, den: 5 }, gate()),
            Err(DispatchError::InvalidConfig)
        );
        assert_eq!(d.video_key_count(), 1 + MAX_DYNAMIC_KEYS);
        let mut bad = cfg(vec![binding(0, 0, 1)], 1, 1);
        bad.video_move_gates = vec![];
        bad.video_move_gates.push(None);
        bad.video_move_gates.push(None);
        assert!(RenditionDispatcher::new(bad).is_err());
    }

    #[test]
    fn pending_fifo_is_bounded_and_drops_with_reported_loss() {
        let now = Instant::now();
        let mut d = RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1)], 1, 1)).unwrap();
        let mut sink = FakeDelivery::new(1);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.rebind_video(0, key, 0, 10).unwrap();
        d.dispatch(ve(key, 0, vec![rap77(0)], Some(mark(TICK))), now, &mut sink)
            .unwrap();
        feed_base(&mut d, &mut sink, 1, 0, now, plain(0)).unwrap();
        assert_eq!(d.binding_video_key(0), Some(key));
        assert_eq!(sink.windows.len(), 1);
        // The dynamic key stalls: base windows keep closing, the FIFO stays bounded.
        for i in 1..=6_i64 {
            feed_base(&mut d, &mut sink, 1, i, now, plain(i)).unwrap();
        }
        assert_eq!(sink.windows.len(), 1);
        assert_eq!(d.counters().dropped_pending, 2);
        assert!(d.bindings[0].pending.len() <= MAX_PENDING_WINDOWS);
        assert!(sink.queues[0].take_discontinuity().is_some());
    }

    #[test]
    fn failed_dynamic_stage_falls_back_to_base_at_a_rap() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(gated_cfg(vec![binding(0, 0, 1), binding(1, 1, 1)], 2, 1))
                .unwrap();
        let mut sink = FakeDelivery::new(2);
        let key = d
            .add_video_key(DtsTimeBase { num: 1, den: 5 }, gate())
            .unwrap();
        d.rebind_video(1, key, 0, 10).unwrap();
        let mut b = Vec::new();
        for i in 0..3_i64 {
            let m = Some(mark(TICK * i128::from(i + 1)));
            let p = if i == 0 { rap77(i) } else { video(i, 8, false) };
            d.dispatch(
                ve(key, u64::try_from(i).unwrap(), vec![p], m),
                now,
                &mut sink,
            )
            .unwrap();
            feed_base(&mut d, &mut sink, 2, i, now, plain(i)).unwrap();
            pop_all(&mut sink, 0, now);
            b.extend(pop_all(&mut sink, 1, now));
        }
        assert_eq!(b.len(), 3);
        assert_eq!(d.binding_video_key(1), Some(key));
        d.take_move_outcomes();
        // Window 3: the dynamic key delivers an unproven packet and wedges.
        d.dispatch(ve(key, 3, vec![video(3, 8, false)], None), now, &mut sink)
            .unwrap();
        feed_base(&mut d, &mut sink, 2, 3, now, plain(3)).unwrap();
        pop_all(&mut sink, 0, now);
        assert_eq!(pop_all(&mut sink, 1, now).len(), 0);
        let later = now + Duration::from_secs(3);
        // Window 4 closes after the stall; its base video for key 1 is a gated RAP.
        feed_base(&mut d, &mut sink, 2, 4, later, |k| {
            if k == 1 {
                rap77(4)
            } else {
                video(4, 8, false)
            }
        })
        .unwrap();
        assert!(d.is_key_failed(key));
        assert_eq!(d.counters().dynamic_failed, 1);
        assert_eq!(d.binding_video_key(1), Some(1));
        assert_eq!(
            d.take_move_outcomes(),
            vec![MoveOutcome::Fallback { output_index: 1 }]
        );
        // Window 3 (a P frame after the switch) is lost; window 4 starts at the RAP.
        assert_eq!(pop_all(&mut sink, 1, later), vec![(77, 2)]);
        assert!(!sink.aborted);
        // Output A was never disturbed.
        assert_eq!(sink.windows.iter().filter(|w| w.0 == 0).count(), 5);
    }

    /// A 48 kHz program-audio config at 25 fps (1920 samples per window).
    fn aac_cfg(bindings: Vec<BindingConfig>) -> DispatchConfig {
        let mut c = cfg(bindings, 1, 1);
        c.audio_time_base = vec![DtsTimeBase {
            num: 1,
            den: 48_000,
        }];
        c.video_time_base = vec![DtsTimeBase { num: 1, den: 25 }];
        c.cadence = Rational::new(25, 1);
        c
    }
    fn tick_ns(i: i64) -> i128 {
        40_000_000 * i128::from(i)
    }
    /// One 25 fps base video window `i`.
    fn ve25(i: i64) -> DispatchEvent {
        DispatchEvent::Video {
            key_index: 0,
            tick: u64::try_from(i).unwrap(),
            timebase: Rational::new(1, 25),
            packets: vec![video(i, 8, i == 0)],
            watermark: Some(mark(tick_ns(i + 1))),
        }
    }
    /// The 1024-sample frames whose DTS lies in window `i` (1920/window).
    fn frames_in(i: i64) -> Vec<i64> {
        let lo = (i * 1920 + 1023) / 1024 * 1024;
        (lo..(i + 1) * 1920).step_by(1024).collect()
    }
    fn tagged(dts: i64, tag: u8) -> EncodedPacket {
        let mut raw = Packet::copy(&[tag; 4]);
        raw.set_pts(Some(dts));
        raw.set_dts(Some(dts));
        EncodedPacket::with_kind(raw, StreamKind::Audio)
    }
    fn audio_event(i: i64, tag: u8) -> DispatchEvent {
        DispatchEvent::Audio {
            tick: u64::try_from(i).unwrap(),
            timebases: vec![Rational::new(1, 48_000)],
            packets: frames_in(i).into_iter().map(|d| tagged(d, tag)).collect(),
            watermarks: vec![Some(mark(tick_ns(i + 1)))],
        }
    }
    /// Every delivered audio packet of output `o`: (dts, tag).
    fn audio_of(sink: &mut FakeDelivery, o: usize, now: Instant) -> Vec<(i64, u8)> {
        let mut out = Vec::new();
        while let Ok(Some(lease)) = sink.queues[o].pop_at(now) {
            for p in lease.packets() {
                if p.kind() == StreamKind::Audio {
                    out.push((p.dts().unwrap(), p.payload().unwrap()[0]));
                }
            }
        }
        out
    }

    #[test]
    fn rebind_audio_splits_the_window_at_g_with_nothing_duplicated_or_missing() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(aac_cfg(vec![binding(0, 0, 1), binding(1, 0, 1)])).unwrap();
        let mut sink = FakeDelivery::new(2);
        let (key, _) = d.add_audio_key().unwrap();
        // G inside window 5 (9600..11520): 10240.
        let g = 10_240;
        let mut seen = [Vec::new(), Vec::new()];
        for i in 0..10_i64 {
            // The private bus runs from window 2 on, carrying its own tag.
            if i >= 2 {
                let p: Vec<_> = frames_in(i).into_iter().map(|x| tagged(x, 2)).collect();
                d.dispatch_dynamic_audio(key, p, Some(mark(tick_ns(i + 1))), false, now, &mut sink)
                    .unwrap();
            }
            if i == 3 {
                d.rebind_audio(1, Some(key), g).unwrap();
            }
            d.dispatch(ve25(i), now, &mut sink).unwrap();
            d.dispatch(audio_event(i, 1), now, &mut sink).unwrap();
            for (o, out) in seen.iter_mut().enumerate() {
                out.extend(audio_of(&mut sink, o, now));
            }
        }
        let all: Vec<i64> = (0..10).flat_map(frames_in).collect();
        // Output 0: the program stage only, untouched.
        assert_eq!(seen[0].iter().map(|p| p.0).collect::<Vec<_>>(), all);
        assert!(seen[0].iter().all(|p| p.1 == 1));
        // Output 1: every DTS exactly once; old (< G) then private (>= G).
        assert_eq!(seen[1].iter().map(|p| p.0).collect::<Vec<_>>(), all);
        for &(dts, tag) in &seen[1] {
            assert_eq!(tag, if dts < g { 1 } else { 2 }, "dts {dts}");
        }
        assert_eq!(d.binding_audio_key(1), Some(key));
        assert!(d.take_move_outcomes().contains(&MoveOutcome::AudioApplied {
            output_index: 1,
            key: Some(key),
            from_dts: g
        }));
    }

    #[test]
    fn a_stalled_dynamic_audio_stage_never_delays_base_windows_and_fails_alone() {
        let now = Instant::now();
        let mut d =
            RenditionDispatcher::new(aac_cfg(vec![binding(0, 0, 1), binding(1, 0, 1)])).unwrap();
        let mut sink = FakeDelivery::new(2);
        let (key, _) = d.add_audio_key().unwrap();
        // The private stage proves window 0 and then wedges.
        d.dispatch_dynamic_audio(
            key,
            vec![tagged(0, 2)],
            Some(mark(tick_ns(1))),
            false,
            now,
            &mut sink,
        )
        .unwrap();
        d.rebind_audio(1, Some(key), 1024).unwrap();
        let mut seen = [Vec::new(), Vec::new()];
        for i in 0..12_i64 {
            d.dispatch(ve25(i), now, &mut sink).unwrap();
            d.dispatch(audio_event(i, 1), now, &mut sink).unwrap();
            // Every base window closes on time for output 0.
            assert_eq!(
                sink.windows.iter().filter(|w| w.0 == 0).count(),
                usize::try_from(i + 1).unwrap(),
                "window {i} delayed"
            );
            for (o, out) in seen.iter_mut().enumerate() {
                out.extend(audio_of(&mut sink, o, now));
            }
        }
        let all: Vec<i64> = (0..12).flat_map(frames_in).collect();
        assert_eq!(seen[0].iter().map(|p| p.0).collect::<Vec<_>>(), all);
        assert!(d.is_audio_key_failed(key), "the stalled stage failed alone");
        assert_eq!(
            d.binding_audio_key(1),
            None,
            "output 1 back on the program stage"
        );
        // Output 1 held at most MAX_PENDING_WINDOWS while the stage stalled,
        // then continued on the program stage.
        assert!(seen[1].len() + MAX_PENDING_WINDOWS * 2 >= all.len());
        assert!(seen[1].iter().all(|p| p.1 == 1));
        assert!(!sink.aborted);
        // Watchdog: a stage that never proves a window fails within 2 s.
        let (k2, _) = d.add_audio_key().unwrap();
        d.rebind_audio(0, Some(k2), 30_720).unwrap();
        d.watchdog(now + STAGE_DEADLINE + Duration::from_millis(10), &mut sink)
            .unwrap();
        assert!(d.is_audio_key_failed(k2));
    }

    /// Sample-exact across the move, with the real libav AAC encoder and
    /// decoder at 48 kHz: the old bus and the private bus encode the same PCM
    /// on the same global grid (the private bus opened mid-run at G0), the
    /// output switches at G, and the decoded output equals decoding the old
    /// bus alone (identical signal on both sides, Rule 2) to within the
    /// codec's own steady-state error, with no duplicated or missing frame.
    #[test]
    #[allow(clippy::too_many_lines, clippy::many_single_char_names)] // reason: one linear end-to-end scenario; d/c/g are the design's names.
    fn rebind_audio_with_real_aac_is_sample_continuous_across_the_move() {
        use multiview_audio::cadence::SampleClock;
        use multiview_audio::crossfade::{grid_ceil, AAC_FRAME_SAMPLES};
        use multiview_output::{AudioEncodeConfig, AudioRenditionEncoder};
        let cadence = Rational::new(25, 1);
        let clock = SampleClock::new(48_000, cadence);
        let config = AudioEncodeConfig::aac(48_000, 2, 128_000);
        let mut program = AudioRenditionEncoder::new(&config).unwrap();
        let tb = program.time_base();
        let mut c = cfg(vec![binding(0, 0, 1), binding(1, 0, 1)], 1, 1);
        c.audio_time_base = vec![DtsTimeBase {
            num: tb.num,
            den: tb.den,
        }];
        c.cadence = cadence;
        c.video_time_base = vec![DtsTimeBase { num: 1, den: 25 }];
        let mut d = RenditionDispatcher::new(c).unwrap();
        let mut sink = FakeDelivery::new(2);
        let (key, _) = d.add_audio_key().unwrap();
        let pcm = |n: u64| -> f32 {
            let t = f32::from(u16::try_from(n % 48_000).unwrap()) / 48_000.0;
            0.3 * (std::f32::consts::TAU * 440.0 * t).sin()
        };
        let admit = 10_u64;
        let g0 = grid_ceil(clock.total_at(admit), AAC_FRAME_SAMPLES);
        let mut private =
            AudioRenditionEncoder::starting_at_sample(&config, i64::try_from(g0).unwrap()).unwrap();
        let move_tick = 25_u64;
        let g = i64::try_from(grid_ceil(clock.total_at(move_tick), AAC_FRAME_SAMPLES)).unwrap();
        let mut seen: Vec<Vec<EncodedPacket>> = vec![Vec::new(), Vec::new()];
        let watermark = |packets: &[EncodedPacket]| {
            packets.last().and_then(|p| {
                crate::rendition_workers::next_frame_lower_bound_ns(p.pts()?, 1024, 48_000)
            })
        };
        let mut private_pending: Vec<EncodedPacket> = Vec::new();
        for tick in 0..60_u64 {
            let (a, b) = (clock.total_at(tick), clock.total_at(tick + 1));
            let block: Vec<f32> = (a..b).flat_map(|n| [pcm(n), pcm(n)]).collect();
            let frames = usize::try_from(b - a).unwrap();
            let pk = program.encode_interleaved(&block, frames).unwrap();
            if tick >= admit {
                // The private bus is fed from its grid start g0.
                let from = g0.max(a);
                let part: Vec<f32> = (from..b).flat_map(|n| [pcm(n), pcm(n)]).collect();
                let mut out = private
                    .encode_interleaved(&part, usize::try_from(b - from).unwrap())
                    .unwrap();
                // A live-opened bus's priming packet is never delivered.
                out.retain(|p| p.dts().is_some_and(|x| x >= i64::try_from(g0).unwrap()));
                private_pending.extend(out);
                let wm = watermark(&private_pending);
                let batch = std::mem::take(&mut private_pending);
                d.dispatch_dynamic_audio(key, batch, wm, false, Instant::now(), &mut sink)
                    .unwrap();
            }
            if tick == move_tick {
                d.rebind_audio(1, Some(key), g).unwrap();
            }
            let t = tick;
            d.dispatch(
                ve25(i64::try_from(tick).unwrap()),
                Instant::now(),
                &mut sink,
            )
            .unwrap();
            let wm = watermark(&pk);
            d.dispatch(
                DispatchEvent::Audio {
                    tick: t,
                    timebases: vec![tb],
                    packets: pk,
                    watermarks: vec![wm],
                },
                Instant::now(),
                &mut sink,
            )
            .unwrap();
            for (queue, out) in sink.queues.iter_mut().zip(seen.iter_mut()) {
                while let Ok(Some(lease)) = queue.pop_at(Instant::now()) {
                    for p in lease.packets() {
                        if p.kind() == StreamKind::Audio {
                            out.push(p.clone_shared_ref().unwrap());
                        }
                    }
                }
            }
        }
        let dts = |v: &[EncodedPacket]| v.iter().map(|p| p.dts().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            dts(&seen[1]),
            dts(&seen[0]),
            "same frames, none duplicated/missing"
        );
        for pair in dts(&seen[1]).windows(2) {
            assert_eq!(pair[1] - pair[0], 1024, "PTS step constant across the move");
        }
        assert!(dts(&seen[1]).contains(&g), "G delivered");
        assert_eq!(
            d.binding_audio_key(1),
            Some(key),
            "output 1 moved to the private bus"
        );
        assert_eq!(d.binding_audio_key(0), None);
        // Output 1 carries private packets from G (they differ in bytes
        // only if the encoders' states differ; the decision is the decode).
        let decode = |packets: &[EncodedPacket]| -> Vec<f32> {
            let params = program.codec_params();
            let mut dec =
                multiview_ffmpeg::StreamAudioDecoder::from_codec_parameters(params, tb).unwrap();
            let mut out = Vec::new();
            for p in packets {
                out.extend(dec.decode_interleaved(p).unwrap());
            }
            out
        };
        let reference = decode(&seen[0]);
        let moved = decode(&seen[1]);
        assert_eq!(
            moved.len(),
            reference.len(),
            "no sample duplicated or missing"
        );
        // Steady-state codec error: two encodes of the same signal on the
        // same grid; measured on the reference's own frame-to-frame error
        // against the PCM fed, the move adds no step above it.
        let gi = usize::try_from(g).unwrap() * 2;
        let steady = reference
            .iter()
            .zip(&moved)
            .take(gi.saturating_sub(4096))
            .fold(0.0_f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(steady < 1e-6, "identical before the move: {steady}");
        let across = reference
            .iter()
            .zip(&moved)
            .skip(gi.saturating_sub(4096))
            .fold(0.0_f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(
            across < 0.05,
            "decoded across G within codec error: {across}"
        );
        let max_step = |v: &[f32]| {
            v.chunks(2)
                .map(|c| c[0])
                .collect::<Vec<_>>()
                .windows(2)
                .fold(0.0_f32, |m, w| m.max((w[1] - w[0]).abs()))
        };
        assert!(
            max_step(&moved) <= max_step(&reference) * 1.1 + 1e-3,
            "no sample step above the steady-signal maximum"
        );
        assert_eq!(
            private.codec_params().codec_config_bytes(),
            program.codec_params().codec_config_bytes(),
            "AudioSpecificConfig byte-identical"
        );
    }
}
