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

const MAX_STAGED_WINDOWS: usize = 16;
const MAX_STAGED_PACKETS: usize = 512;
const MAX_STAGED_BYTES: usize = 16 * 1024 * 1024;
const STAGE_DEADLINE: Duration = Duration::from_secs(2);

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
            .checked_div(i128::from(self.den))
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
        let index = dts
            .checked_mul(i128::from(cadence.num))
            .and_then(|v| v.checked_div(i128::from(cadence.den) * 1_000_000_000))
            .ok_or(DispatchError::InvalidEvent)?;
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

/// Per-program bounded join. The caller handles returned errors and never
/// labels an output healthy after a rejected event or missing proof.
pub(crate) struct RenditionDispatcher {
    bindings: Vec<BindingConfig>,
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
            bindings.push(binding);
        }
        Ok(Self {
            bindings,
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
        if let Err(error) = self.check_global_budget(&event) {
            self.fail();
            delivery.abort();
            return Err(error);
        }
        let result = self
            .apply(event, now)
            .and_then(|()| self.drain(now, delivery));
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
    fn apply(&mut self, event: DispatchEvent, now: Instant) -> Result<(), DispatchError> {
        match event {
            DispatchEvent::Video {
                key_index,
                tick,
                timebase,
                packets,
                watermark,
            } => {
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
    fn drain<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        if self.video.iter().any(|s| s.stale(now)) || self.audio.iter().any(|s| s.stale(now)) {
            return Err(DispatchError::WatchdogExpired);
        }
        loop {
            let Some(index) = self.next_window else {
                return Ok(());
            };
            let live: Vec<_> = self
                .video
                .iter()
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
                .chain(&self.audio)
                .filter_map(|s| s.windows.back().map(|w| w.index))
                .max();
            if max_index.is_none_or(|last| index > last)
                && self.video.iter().chain(&self.audio).all(|s| s.finished)
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
            if !self.video.iter().all(|stage| stage.complete(end))
                || !self.audio.iter().all(|stage| stage.complete(end))
            {
                return Ok(());
            }
            let video: Vec<Vec<_>> = self.video.iter_mut().map(|s| s.take(index)).collect();
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
            let mut first_admission_error = None;
            for binding in &mut self.bindings {
                let mut packets = Vec::new();
                if let Some(v) = binding.video_key {
                    for p in &video[v] {
                        packets.push(
                            p.clone_shared_ref()
                                .map_err(|_| DispatchError::PacketReference)?,
                        );
                    }
                }
                for &a in &binding.audio_tracks {
                    for p in &audio[a] {
                        packets.push(
                            p.clone_shared_ref()
                                .map_err(|_| DispatchError::PacketReference)?,
                        );
                    }
                }
                let start =
                    Self::tick_boundary(index, self.cadence).ok_or(DispatchError::InvalidEvent)?;
                let media_span = Duration::from_nanos(
                    u64::try_from(end.checked_sub(start).ok_or(DispatchError::InvalidEvent)?)
                        .map_err(|_| DispatchError::InvalidEvent)?,
                );
                if delivery
                    .offer_window(
                        binding.output_index,
                        delivery.generation(binding.output_index),
                        packets,
                        media_span,
                        now,
                    )
                    .is_err()
                {
                    first_admission_error.get_or_insert(DispatchError::Admission {
                        output_index: binding.output_index,
                    });
                }
            }
            if let Some(error) = first_admission_error {
                return Err(error);
            }
            if self
                .video
                .iter()
                .chain(&self.audio)
                .all(|s| s.finished && s.windows.is_empty())
            {
                self.finished = true;
                return Ok(());
            }
            if self
                .video
                .iter()
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
    pub(crate) fn watchdog<D: WindowDelivery>(
        &mut self,
        now: Instant,
        delivery: &mut D,
    ) -> Result<(), DispatchError> {
        if self.video.iter().any(|s| s.stale(now)) || self.audio.iter().any(|s| s.stale(now)) {
            self.fail();
            delivery.abort();
            Err(DispatchError::WatchdogExpired)
        } else {
            Ok(())
        }
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
        }
    }
    fn binding(i: usize, v: usize, a: usize) -> BindingConfig {
        BindingConfig {
            output_index: i,
            video_key: Some(v),
            audio_tracks: (0..a).collect(),
        }
    }
    fn mark(ns: i128) -> Option<DtsWatermark> {
        Some(DtsWatermark {
            complete_before_ns: ns,
        })
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
        d.dispatch(ae(0, vec![audio(0, 0)], vec![mark(TICK)]), now, &mut sink)
            .unwrap();
        assert!(sink.windows.is_empty());
        d.dispatch(
            ve(
                0,
                1,
                (0..5).map(|_| video(0, 6, false)).collect(),
                mark(TICK - 1),
            ),
            now,
            &mut sink,
        )
        .unwrap();
        assert!(sink.windows.is_empty());
        d.dispatch(ve(0, 2, vec![], mark(TICK)), now, &mut sink)
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
            ve(0, 0, vec![video(0, 8, true)], mark(TICK)),
            now,
            &mut sink,
        )
        .unwrap();
        let event = DispatchEvent::Audio {
            tick: 0,
            timebases: vec![Rational::new(1, 5), Rational::new(1, 10)],
            packets: vec![audio(0, 0), audio(0, 1)],
            watermarks: vec![mark(TICK), None],
        };
        d.dispatch(event, now, &mut sink).unwrap();
        assert!(sink.windows.is_empty()); // unproven discrete track gates BOTH streams
        let event = DispatchEvent::Audio {
            tick: 1,
            timebases: vec![Rational::new(1, 5), Rational::new(1, 10)],
            packets: vec![],
            watermarks: vec![mark(TICK), mark(TICK)],
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
            ve(0, 0, vec![video(0, 8, true)], mark(TICK)),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            ve(1, 0, vec![video(0, 8, true)], mark(TICK)),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(ae(0, vec![audio(0, 0)], vec![mark(TICK)]), now, &mut sink)
            .unwrap();
        assert_eq!(sink.windows, vec![(0, 2), (1, 2), (2, 2)]);
        let held = sink.queues[1].pop_at(now).unwrap().unwrap();
        sink.queues[1] = BoundedBatchQueue::with_limits(1, 1, Duration::from_secs(2))
            .with_recovery_gate(RecoveryGate::StrictRap {
                codec: CodecKind::H264,
                framing: NalFraming::AnnexB,
            });
        d.dispatch(
            ve(0, 1, vec![video(1, 8, false)], mark(TICK * 2)),
            now,
            &mut sink,
        )
        .unwrap();
        d.dispatch(
            ve(1, 1, vec![video(1, 8, false)], mark(TICK * 2)),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            d.dispatch(
                ae(1, vec![audio(1, 0)], vec![mark(TICK * 2)]),
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
            ve(0, 0, vec![video(0, 8, true)], mark(TICK)),
            now,
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            d.dispatch(
                ae(
                    0,
                    (0..5).map(|i| audio(0, i)).collect(),
                    vec![mark(TICK); 5]
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
            ve(0, 0, vec![video(0, 11 * 1024 * 1024, false)], mark(TICK)),
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
            Some(gate),
        );
        let mut d = RenditionDispatcher::new(cfg(
            vec![binding(0, 0, 1), binding(1, 0, 1), binding(2, 0, 1)],
            1,
            1,
        ))
        .unwrap();
        d.dispatch(
            ve(0, 0, vec![video(0, 6, true)], mark(TICK)),
            now,
            &mut delivery,
        )
        .unwrap();
        d.dispatch(
            ae(0, vec![audio(0, 0)], vec![mark(TICK)]),
            now,
            &mut delivery,
        )
        .unwrap();
        let mut pointers = Vec::new();
        for rx in &receivers {
            let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let second = rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(first.kind(), StreamKind::Video);
            assert_eq!(second.kind(), StreamKind::Audio);
            pointers.push(first.payload().unwrap().as_ptr());
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
}
