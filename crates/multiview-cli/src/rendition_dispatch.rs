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
    },
    Refused {
        output_index: usize,
        reason: MoveRefusal,
    },
    /// A pending move was replaced by a newer request.
    Superseded { output_index: usize },
    /// The binding's dynamic key failed/retired; it returned to its startup key.
    Fallback { output_index: usize },
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
}

#[derive(Clone, Copy)]
struct PendingMove {
    key: usize,
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
            });
        }
        let mut gates = config.video_move_gates.into_iter();
        let video_meta = (0..config.video_count)
            .map(|_| VideoMeta {
                dynamic: false,
                retired: false,
                failed: false,
                gate: gates.next().flatten(),
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
                    .is_none_or(|x| x.failed || x.retired)
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
    }
    fn pump<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<Option<DispatchError>, DispatchError> {
        let mut first = None;
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
            let Some(mut entry) = b.pending.pop_front() else {
                break;
            };
            let index = entry.index;
            let mut applied = false;
            if let Some(mut m) = b.mv {
                if index >= m.at_tick {
                    let usable = self
                        .video_meta
                        .get(m.key)
                        .is_some_and(|x| !x.failed && !x.retired);
                    if usable {
                        let dest_dynamic = self.video_meta.get(m.key).is_some_and(|x| x.dynamic);
                        let first_pkt_passes = |gate: &Option<MoveGate>| -> bool {
                            let Some(g) = gate.as_ref() else {
                                return false;
                            };
                            if dest_dynamic {
                                self.video.get(m.key).is_some_and(|s| s.complete(end))
                                    && self
                                        .video
                                        .get(m.key)
                                        .and_then(|s| s.peek(index))
                                        .and_then(|w| w.packets.first())
                                        .is_some_and(|p| g(p))
                            } else {
                                entry
                                    .base
                                    .iter()
                                    .find(|(k, _)| *k == m.key)
                                    .and_then(|(_, v)| v.first())
                                    .is_some_and(g)
                            }
                        };
                        let pass = self
                            .video_meta
                            .get(m.key)
                            .is_some_and(|x| first_pkt_passes(&x.gate));
                        if pass {
                            b.video_key = Some(m.key);
                            push_outcome(
                                &mut self.outcomes,
                                MoveOutcome::Applied {
                                    output_index: b.output_index,
                                    key: m.key,
                                    tick: u64::try_from(index).unwrap_or(u64::MAX),
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
                let pass = applied || gate.is_none_or(|g| packets.first().is_some_and(g));
                if pass {
                    b.fallback_gate_pending = false;
                } else {
                    // Never a mid-GOP picture after a switch: the whole window is lost.
                    delivery.report_loss(b.output_index, OfferError::ProducerLoss);
                    self.counters.dropped_pending = self.counters.dropped_pending.saturating_add(1);
                    continue;
                }
            }
            packets.append(&mut entry.audio);
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
                at_tick: i128::from(at_tick),
                max_windows,
                failed: 0,
            });
        }
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
                tick: 4
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
}
