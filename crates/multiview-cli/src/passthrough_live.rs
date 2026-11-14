//! `PassthroughLive`: a routing key that passes one source's compressed video
//! through to an output slot (output-routing §12.5, issue #344 D2a).
//!
//! It is a [`LiveEncoder`] so the router and dispatcher treat it exactly like
//! an encoded key (ADR-0108 moves, parameter-set comparison, HLS
//! discontinuity, linger, readback). Per [`VideoEncodeWorker::encode`] call it
//! ignores the canvas and returns **exactly one** video packet for
//! `item.tick`, from the D1 [`PassthroughClock`]:
//!
//! * the source AU, re-stamped `dts = tick` and `pts = tick + reorder +
//!   delay` (the source's own reorder, plus the clock's bounded presentation
//!   delay);
//! * a repeat AU (H.264 drift correction, one [`RepeatAuBuilder`] per
//!   parameter generation, built once);
//! * the **format slate** AU: a closed black GOP baked once at open with the
//!   slot's own encoder (`VideoRenditionEncoder` at the slot format), after
//!   which no encoder session is held. Entering the slate always starts at
//!   its IDR.
//!
//! `GapFill` already calls `encode` for every tick a dropped offer missed, so
//! one AU per tick holds across drops (invariant #1).
//!
//! **Never blocking.** `encode` drains the tap with `try_recv` only (bounded
//! per tick by the clock), takes no lock and never waits; the bake happens on
//! the routing-open thread inside the opener, never on the worker.
//!
//! **Watermark.** DTS is the tick for every packet, so the worker proves
//! [`ReorderBound::DtsIsTick`]: after the call for tick `T` no packet with
//! `DTS < T + 1` can follow, whatever the PTS (B-frame reorder rides PTS).
//!
//! D2b seams: the router's P0 decision opens this through a passthrough
//! opener, reads [`PassthroughStatus`] for the readback / picture bridge, and
//! the audio rail reads [`PassthroughStatus::delay_ticks`].

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use ffmpeg_next::codec::packet::{Flags, Packet};
use multiview_compositor::pipeline::Nv12Image;
use multiview_core::time::Rational;
use multiview_ffmpeg::h264_repeat::RepeatAuBuilder;
use multiview_ffmpeg::idr::{self, CodecKind, NalFraming, ParameterSetScan, RapParameterSets};
use multiview_ffmpeg::EncodedPacket;
use multiview_output::passthrough::{
    BridgeReason, ClockState, Emission, PassthroughClock, RepeatSource, SlateLoop, SlotGate, TapAu,
    TapReader, MAX_AU_BYTES,
};

use crate::output_routing::LiveEncoder;
use crate::pipeline::PipelineError;
use crate::rendition_dispatch::{MoveGate, ParameterSets};
use crate::rendition_workers::{BakedTick, ReorderBound, VideoEncodeWorker};

/// Most packets a slate bake keeps (one closed GOP).
pub const MAX_SLATE_PACKETS: usize = 300;

/// What the key emitted on its last tick ([`PassthroughStatus::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassthroughState {
    /// No tick emitted yet.
    Idle,
    /// A source AU (path P0).
    Passthrough,
    /// A synthesised repeat AU (still P0).
    Repeat,
    /// The format slate (bridging).
    Slate,
}

impl PassthroughState {
    const fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Passthrough => 1,
            Self::Repeat => 2,
            Self::Slate => 3,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Passthrough,
            2 => Self::Repeat,
            3 => Self::Slate,
            _ => Self::Idle,
        }
    }

    /// Kebab-case name for the readback.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Passthrough => "passthrough",
            Self::Repeat => "repeat",
            Self::Slate => "slate",
        }
    }
}

/// The bridge reason codes on [`PassthroughStatus::reason`] (0 = none).
fn reason_code(reason: BridgeReason) -> u8 {
    match reason {
        BridgeReason::NotArmed => 1,
        BridgeReason::AwaitingIdr => 2,
        BridgeReason::Starting => 3,
        BridgeReason::Stall => 4,
        BridgeReason::Late => 5,
        BridgeReason::JitterOverflow => 6,
        BridgeReason::ParameterChange => 7,
        BridgeReason::ParametersNoLongerMatch => 8,
        BridgeReason::SlateNotReady => 9,
        BridgeReason::Malformed => 10,
        BridgeReason::PresentationDelay => 11,
    }
}

/// The bridge reason for a [`PassthroughStatus::reason`] code.
#[must_use]
pub fn reason_from_code(code: u8) -> Option<BridgeReason> {
    Some(match code {
        1 => BridgeReason::NotArmed,
        2 => BridgeReason::AwaitingIdr,
        3 => BridgeReason::Starting,
        4 => BridgeReason::Stall,
        5 => BridgeReason::Late,
        6 => BridgeReason::JitterOverflow,
        7 => BridgeReason::ParameterChange,
        8 => BridgeReason::ParametersNoLongerMatch,
        9 => BridgeReason::SlateNotReady,
        10 => BridgeReason::Malformed,
        11 => BridgeReason::PresentationDelay,
        _ => return None,
    })
}

/// Lock-free readback of one passthrough key, written by its worker thread
/// once per tick and read by the router / readback / audio (D2b).
#[derive(Debug, Default)]
pub struct PassthroughStatus {
    state: AtomicU8,
    reason: AtomicU8,
    /// Ticks between the emitted AU's source time and the tick (the clock's
    /// jitter-buffer depth at emission; 0 on the slate).
    delay_ticks: AtomicU32,
    /// The last tick emitted.
    last_tick: AtomicU64,
    /// Source AUs emitted.
    pub aus: AtomicU64,
    /// Repeat AUs emitted.
    pub repeats: AtomicU64,
    /// Slate ticks emitted.
    pub slate_ticks: AtomicU64,
    /// Runs started (moves into P0 inside the key).
    pub runs: AtomicU64,
    /// Runs ended by a stall.
    pub stalls: AtomicU64,
    /// Runs ended by a parameter change.
    pub param_changes: AtomicU64,
    /// AUs dropped by drift corrections.
    pub drift_drops: AtomicU64,
}

impl PassthroughStatus {
    /// What the last tick emitted.
    #[must_use]
    pub fn state(&self) -> PassthroughState {
        PassthroughState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Why the last tick bridged (`None` while passing through).
    #[must_use]
    pub fn reason(&self) -> Option<BridgeReason> {
        reason_from_code(self.reason.load(Ordering::Acquire))
    }

    /// The emitted AU's source-time delay in ticks (0 on the slate).
    #[must_use]
    pub fn delay_ticks(&self) -> u32 {
        self.delay_ticks.load(Ordering::Acquire)
    }

    /// The last tick emitted.
    #[must_use]
    pub fn last_tick(&self) -> u64 {
        self.last_tick.load(Ordering::Acquire)
    }
}

/// The baked format slate: one closed GOP, every packet a forward-predicted
/// picture, the first one an IDR with its parameter sets in band.
#[derive(Debug, Clone)]
pub struct FormatSlate {
    packets: Arc<[Arc<[u8]>]>,
}

impl FormatSlate {
    /// A slate from encoded packets (Annex B). `None` unless the first packet
    /// is a self-contained IDR of `codec`.
    #[must_use]
    pub fn new(packets: Vec<Vec<u8>>, codec: CodecKind) -> Option<Self> {
        let first = packets.first()?;
        if !idr::is_self_contained_idr(first, codec, NalFraming::AnnexB) {
            return None;
        }
        Some(Self {
            packets: packets.into_iter().map(Arc::from).collect(),
        })
    }

    /// The slate's packets (IDR first).
    #[must_use]
    pub fn packets(&self) -> &[Arc<[u8]>] {
        &self.packets
    }
}

/// Bake a format slate once with the slot's own encoder settings: encode
/// `gop` black pictures at `cfg`'s format and keep the packets up to (not
/// including) the second keyframe. The encoder is dropped before returning:
/// no session is held after the bake. Runs on the routing-open thread.
///
/// # Errors
/// The encoder cannot open or encode, or the result is not one closed GOP
/// led by a self-contained IDR.
pub fn bake_slate(
    cfg: &multiview_output::sink::EncodeConfig,
    black: &Nv12Image,
    codec: CodecKind,
) -> Result<FormatSlate, String> {
    let mut encoder =
        multiview_output::sink::VideoRenditionEncoder::new(cfg).map_err(|e| e.to_string())?;
    if !encoder.no_reorder_proven() {
        return Err("slate encoder may reorder".to_owned());
    }
    let framing = encoder
        .codec_params()
        .nal_framing()
        .ok_or_else(|| "slate encoder framing unknown".to_owned())?;
    let gop = usize::try_from(cfg.gop.max(1))
        .unwrap_or(1)
        .min(MAX_SLATE_PACKETS);
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(gop);
    let push = |packets: Vec<EncodedPacket>, out: &mut Vec<Vec<u8>>| -> bool {
        for p in packets {
            let Some(bytes) = p.payload() else { continue };
            if p.is_keyframe() && !out.is_empty() {
                return true;
            }
            if let Some(annexb) = idr::to_annex_b(bytes, framing, MAX_AU_BYTES) {
                out.push(annexb.to_vec());
            }
            if out.len() >= gop {
                return true;
            }
        }
        false
    };
    let mut done = false;
    for tick in 0..u64::try_from(gop).unwrap_or(1) {
        let frame = crate::pipeline::nv12_to_decoded(black).map_err(|e| e.to_string())?;
        let packets = encoder
            .encode_frame_at(frame, tick)
            .map_err(|e| e.to_string())?;
        if push(packets, &mut out) {
            done = true;
            break;
        }
    }
    if !done {
        let tail = encoder.finish().map_err(|e| e.to_string())?;
        push(tail, &mut out);
    }
    drop(encoder);
    FormatSlate::new(out, codec).ok_or_else(|| "slate is not IDR-led".to_owned())
}

/// Entries in a key's [`RapRing`]. The dispatcher evaluates a move gate only
/// on windows it still stages (at most 16 ticks back), so 32 gated DTS values
/// always cover them; an overwritten entry is only ever a missed move point
/// (the move waits for the next one), never a false RAP.
const RAP_RING: usize = 32;

/// The DTS values of this key's move points (run-start strict IDRs and slate
/// IDRs), written by the worker thread **before** the packet is emitted
/// (Release) and read by the dispatcher's move gate (Acquire). Lock-free;
/// one writer.
#[derive(Debug)]
struct RapRing {
    dts: [AtomicI64; RAP_RING],
    next: AtomicUsize,
}

impl RapRing {
    fn new() -> Self {
        Self {
            dts: std::array::from_fn(|_| AtomicI64::new(i64::MIN)),
            next: AtomicUsize::new(0),
        }
    }
    fn record(&self, dts: i64) {
        let i = self.next.load(Ordering::Relaxed);
        if let Some(slot) = self.dts.get(i % RAP_RING) {
            slot.store(dts, Ordering::Release);
        }
        self.next.store(i.wrapping_add(1), Ordering::Relaxed);
    }
    fn contains(&self, dts: i64) -> bool {
        dts != i64::MIN && self.dts.iter().any(|d| d.load(Ordering::Acquire) == dts)
    }
}

/// What a passthrough key needs to know about its slot and source.
#[derive(Debug, Clone)]
pub struct PassthroughSpec {
    /// The output cadence (the time base is `1/cadence`).
    pub cadence: Rational,
    /// The source codec (H.264; HEVC passes with no repeat AU).
    pub codec: CodecKind,
    /// The stream parameters a decoder-config change hands the muxer (D2b:
    /// from the source's extradata; `None` keeps the muxer's own seed).
    pub stream_params: Option<multiview_ffmpeg::StreamCodecParameters>,
}

/// The slot gate a key uses until D2b's router supplies the match verdict:
/// parameters match when the key is told so ([`PassthroughLive::set_matching`]),
/// and the matched slate is the format slate, ready from open.
#[derive(Debug, Default)]
struct KeyGate {
    /// `params_gen` values that match the slot (`None` = every generation).
    matching: Option<u64>,
}

impl SlotGate for KeyGate {
    fn matches(&self, gen: u64) -> bool {
        self.matching.is_none_or(|m| m == gen)
    }
    fn matched_slate_ready(&self, _gen: u64) -> bool {
        true
    }
}

/// One [`RepeatAuBuilder`] per parameter generation, built once.
#[derive(Default)]
struct Repeats {
    codec: Option<CodecKind>,
    built: Option<(u64, Option<RepeatAuBuilder>)>,
}

impl RepeatSource for Repeats {
    fn repeat_after(&mut self, prev: &TapAu) -> Option<Arc<[u8]>> {
        if self.codec != Some(CodecKind::H264) {
            return None;
        }
        // The builder needs this generation's SPS/PPS; the generation's IDR
        // carries them in band (a strict IDR). Built from the first AU of the
        // generation that has both, then reused.
        let rebuild = self
            .built
            .as_ref()
            .is_none_or(|(g, b)| *g != prev.params_gen || b.is_none());
        if rebuild {
            let sps = idr::h264_nal_of_type(&prev.data, NalFraming::AnnexB, 7);
            let pps = idr::h264_nal_of_type(&prev.data, NalFraming::AnnexB, 8);
            match (sps, pps) {
                (Some(sps), Some(pps)) => {
                    self.built = Some((prev.params_gen, RepeatAuBuilder::new(sps, pps)));
                }
                _ if self
                    .built
                    .as_ref()
                    .is_some_and(|(g, _)| *g == prev.params_gen) => {}
                _ => self.built = Some((prev.params_gen, None)),
            }
        }
        let (_, builder) = self.built.as_ref()?;
        builder
            .as_ref()?
            .repeat_after(&prev.data, NalFraming::AnnexB)
            .map(Arc::from)
    }
}

/// A source of monotonic nanoseconds for the clock (the run's time source).
pub type NowNs = Box<dyn Fn() -> u64 + Send>;

/// The passthrough routing key. See the [module docs](self).
pub struct PassthroughLive {
    cadence: Rational,
    codec: CodecKind,
    reader: TapReader,
    clock: PassthroughClock,
    gate: KeyGate,
    repeats: Repeats,
    slate: SlateLoop,
    on_slate: bool,
    status: Arc<PassthroughStatus>,
    raps: Arc<RapRing>,
    now_ns: NowNs,
    stream_params: Option<multiview_ffmpeg::StreamCodecParameters>,
    /// The next tick `encode` must see (strictly increasing).
    next: Option<u64>,
}

impl std::fmt::Debug for PassthroughLive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassthroughLive")
            .field("codec", &self.codec)
            .field("state", &self.status.state())
            .finish_non_exhaustive()
    }
}

/// Owned parameter sets of an Annex-B AU (4-byte BE length + NAL), the
/// encoding `StreamCodecParameters::rap_parameter_sets` uses.
fn sets_of(au: &[u8], codec: CodecKind) -> RapParameterSets {
    match idr::scan_parameter_sets(au, codec, NalFraming::AnnexB) {
        ParameterSetScan::Absent => RapParameterSets::Absent,
        ParameterSetScan::Rejected => RapParameterSets::Rejected,
        ParameterSetScan::Found(sets) => {
            let mut out = Vec::new();
            for set in sets {
                let Ok(len) = u32::try_from(set.len()) else {
                    return RapParameterSets::Rejected;
                };
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(set);
            }
            RapParameterSets::Sets(out)
        }
    }
}

impl PassthroughLive {
    /// A key reading `reader` with the baked `slate`, armed: the first strict
    /// IDR that passes the gate starts a run.
    #[must_use]
    pub fn new(
        spec: PassthroughSpec,
        reader: TapReader,
        slate: &FormatSlate,
        now_ns: NowNs,
    ) -> Option<Self> {
        let loop_ = SlateLoop::new(Arc::clone(&slate.packets))?;
        let mut clock = PassthroughClock::new(spec.cadence);
        clock.arm();
        Some(Self {
            cadence: spec.cadence,
            codec: spec.codec,
            reader,
            clock,
            gate: KeyGate::default(),
            repeats: Repeats {
                codec: Some(spec.codec),
                built: None,
            },
            slate: loop_,
            on_slate: false,
            status: Arc::new(PassthroughStatus::default()),
            raps: Arc::new(RapRing::new()),
            now_ns,
            stream_params: spec.stream_params,
            next: None,
        })
    }

    /// The key's lock-free status.
    #[must_use]
    pub fn status(&self) -> Arc<PassthroughStatus> {
        Arc::clone(&self.status)
    }

    /// Restrict the matching parameter generation (D2b: the router's match
    /// verdict). `None` = every generation matches.
    pub fn set_matching(&mut self, gen: Option<u64>) {
        self.gate.matching = gen;
    }

    /// The clock's path readback.
    #[must_use]
    pub fn clock_state(&self) -> ClockState {
        self.clock.state()
    }

    fn packet(data: &[u8], dts: i64, pts: i64, key: bool) -> EncodedPacket {
        let mut raw = Packet::copy(data);
        raw.set_dts(Some(dts));
        raw.set_pts(Some(pts));
        if key {
            raw.set_flags(Flags::KEY);
        }
        EncodedPacket::from_packet(raw)
    }

    /// One tick: exactly one packet stamped `dts == tick`.
    fn tick(&mut self, tick: u64) -> Result<EncodedPacket, PipelineError> {
        let now = (self.now_ns)();
        let emission = self.clock.on_tick(
            tick,
            now,
            self.reader.drain(),
            &self.gate,
            &mut self.repeats,
        );
        let c = self.clock.counters();
        let s = &self.status;
        s.runs.store(c.runs, Ordering::Relaxed);
        s.stalls.store(c.stalls, Ordering::Relaxed);
        s.param_changes.store(c.param_changes, Ordering::Relaxed);
        s.drift_drops.store(c.drift_drops, Ordering::Relaxed);
        s.last_tick.store(tick, Ordering::Release);
        let depth = u32::try_from(self.clock.last_depth()).unwrap_or(u32::MAX);
        let (packet, state, reason, delay) = match emission {
            Emission::Au {
                data,
                dts,
                pts,
                keyframe,
                run_start,
                ..
            } => {
                self.on_slate = false;
                if run_start
                    && keyframe
                    && idr::is_self_contained_idr(&data, self.codec, NalFraming::AnnexB)
                {
                    self.raps.record(dts);
                }
                s.aus.fetch_add(1, Ordering::Relaxed);
                (
                    Self::packet(&data, dts, pts, keyframe),
                    PassthroughState::Passthrough,
                    0,
                    depth,
                )
            }
            Emission::Repeat { data, dts, pts } => {
                self.on_slate = false;
                s.repeats.fetch_add(1, Ordering::Relaxed);
                (
                    Self::packet(&data, dts, pts, false),
                    PassthroughState::Repeat,
                    0,
                    depth,
                )
            }
            Emission::Bridge { dts, pts, reason } => {
                if !self.on_slate {
                    // Entering the slate always starts at its IDR.
                    self.slate.enter();
                    self.on_slate = true;
                }
                let au = self
                    .slate
                    .next_at(dts, pts)
                    .ok_or_else(|| PipelineError::Output {
                        kind: "passthrough",
                        reason: "slate stamp".to_owned(),
                    })?;
                s.slate_ticks.fetch_add(1, Ordering::Relaxed);
                if au.keyframe {
                    self.raps.record(au.dts);
                }
                (
                    Self::packet(&au.data, au.dts, au.pts, au.keyframe),
                    PassthroughState::Slate,
                    reason_code(reason),
                    0,
                )
            }
        };
        s.state.store(state.code(), Ordering::Release);
        s.reason.store(reason, Ordering::Release);
        s.delay_ticks.store(delay, Ordering::Release);
        Ok(packet)
    }
}

impl VideoEncodeWorker for PassthroughLive {
    fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
        if self.next.is_some_and(|next| item.tick < next) {
            return Err(PipelineError::Output {
                kind: "passthrough",
                reason: format!("tick {} after {:?}", item.tick, self.next),
            });
        }
        self.next = Some(item.tick.saturating_add(1));
        Ok(vec![self.tick(item.tick)?])
    }

    fn reorder_bound(&self) -> ReorderBound {
        ReorderBound::DtsIsTick
    }

    fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
        Ok(Vec::new())
    }
}

impl LiveEncoder for PassthroughLive {
    fn start_at(&mut self, t0: u64) {
        self.next = Some(t0);
    }

    fn time_base(&self) -> Rational {
        Rational::new(self.cadence.den, self.cadence.num)
    }

    /// A RAP iff a run-start strict IDR or the slate's IDR (recorded by the
    /// worker in the key's [`RapRing`] before the packet left it). A mid-run
    /// source IDR keeps its keyframe flag (HLS segments start there) but is
    /// not a move point.
    fn move_gate(&self) -> MoveGate {
        let raps = Arc::clone(&self.raps);
        Box::new(move |packet: &EncodedPacket| {
            packet.is_keyframe() && packet.dts().is_some_and(|dts| raps.contains(dts))
        })
    }

    fn parameter_sets(&self) -> Option<ParameterSets> {
        let codec = self.codec;
        Some(Box::new(move |packet: &EncodedPacket| {
            if !packet.is_keyframe() {
                return RapParameterSets::Absent;
            }
            packet
                .payload()
                .map_or(RapParameterSets::Absent, |bytes| sets_of(bytes, codec))
        }))
    }

    fn stream_params(&self) -> Option<multiview_ffmpeg::StreamCodecParameters> {
        self.stream_params.clone()
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
    use std::sync::Mutex;

    use multiview_output::passthrough::CompressedTap;

    use crate::rendition_workers::{next_tick_lower_bound_ns, DispatchEvent, RenditionWorker};

    const CADENCE: Rational = Rational::new(25, 1);
    const TICK_NS: u64 = 40_000_000;
    const FRAME: i64 = 3_600; // 90 kHz per 25 fps frame
    /// The source's constant reorder (PTS - DTS), in frames.
    const REORDER: i64 = 2;
    const SPS_A: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xb2, 0x00, 0xa0, 0x0b, 0x76, 0x02, 0x20, 0x00, 0x00, 0x03,
        0x00, 0x20, 0x00, 0x00, 0x07, 0x81, 0xe3, 0x06, 0x49,
    ];
    const SPS_B: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xe4, 0x01, 0x40, 0x16, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03,
        0x00, 0x40, 0x00, 0x00, 0x0f, 0x03, 0xc6, 0x0c, 0x44, 0x80,
    ];
    const SPS_SLATE: &[u8] = &[
        0x67, 0x42, 0xc0, 0x0d, 0xda, 0x05, 0x07, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40,
        0x00, 0x00, 0x0f, 0x03, 0xc5, 0x0a, 0xa8,
    ];
    const PPS: &[u8] = &[0x68, 0xeb, 0xc3, 0xcb, 0x22, 0xc0];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [0_u8, 0, 0, 1].into_iter().chain(n.iter().copied()))
            .collect()
    }
    fn idr_au(sps: &[u8], tag: u8) -> Vec<u8> {
        annexb(&[sps, PPS, &[0x65, 0x88, tag]])
    }
    fn p_au(tag: u8) -> Vec<u8> {
        annexb(&[&[0x41, 0x9a, tag]])
    }
    fn slate() -> FormatSlate {
        let mut packets = vec![idr_au(SPS_SLATE, 0xEE)];
        packets.extend((0..9).map(|_| p_au(0xEE)));
        FormatSlate::new(packets, CodecKind::H264).unwrap()
    }

    /// The scripted source: frame `k` arrives on tick `k`, IDR every 30
    /// frames, nothing in `stall`, parameter set B from frame `change`.
    struct Script {
        stall: std::ops::Range<u64>,
        change: u64,
    }

    impl Script {
        fn au(&self, k: u64) -> Option<TapAu> {
            if self.stall.contains(&k) {
                return None;
            }
            let (sps, gen) = if k >= self.change {
                (SPS_B, 2)
            } else {
                (SPS_A, 1)
            };
            let key = k % 30 == 0;
            let tag = u8::try_from(k % 200).unwrap();
            let data = if key { idr_au(sps, tag) } else { p_au(tag) };
            let dts = i64::try_from(k).unwrap() * FRAME;
            Some(TapAu {
                data: Arc::from(data),
                dts,
                pts: dts + REORDER * FRAME,
                time_base: Rational::new(1, 90_000),
                strict_idr: key,
                reference: true,
                params_gen: gen,
                arrival_ns: k * TICK_NS,
            })
        }
    }

    /// One tick's record: the packet stamps, what the key emitted, whether the
    /// move gate passed, and the bytes.
    #[derive(Debug, Clone)]
    struct Rec {
        tick: u64,
        dts: i64,
        pts: i64,
        key: bool,
        gate: bool,
        state: PassthroughState,
        data: Vec<u8>,
        sets: RapParameterSets,
        count: usize,
    }

    /// Feeds the script into the tap, then encodes, recording each tick:
    /// deterministic inside the worker thread.
    struct Feeding {
        tap: CompressedTap,
        script: Script,
        now: Arc<AtomicU64>,
        live: PassthroughLive,
        gate: MoveGate,
        sets: ParameterSets,
        log: Arc<Mutex<Vec<Rec>>>,
    }

    impl VideoEncodeWorker for Feeding {
        fn encode(&mut self, item: BakedTick) -> Result<Vec<EncodedPacket>, PipelineError> {
            let tick = item.tick;
            self.now.store(tick * TICK_NS, Ordering::Release);
            if let Some(au) = self.script.au(tick) {
                self.tap.offer(au);
            }
            let out = self.live.encode(item)?;
            let p = &out[0];
            self.log.lock().unwrap().push(Rec {
                tick,
                dts: p.dts().unwrap(),
                pts: p.pts().unwrap(),
                key: p.is_keyframe(),
                gate: (self.gate)(p),
                state: self.live.status.state(),
                data: p.payload().unwrap().to_vec(),
                sets: (self.sets)(p),
                count: out.len(),
            });
            Ok(out)
        }
        fn reorder_bound(&self) -> ReorderBound {
            self.live.reorder_bound()
        }
        fn finish(&mut self) -> Result<Vec<EncodedPacket>, PipelineError> {
            self.live.finish()
        }
    }

    /// A run's per-tick records, worker watermarks and the key's status.
    type Outcome = (Vec<Rec>, Vec<(u64, Option<i128>)>, Arc<PassthroughStatus>);

    fn run(ticks: u64, script: Script) -> Outcome {
        let tap = CompressedTap::new();
        let reader = tap.subscribe().unwrap();
        let now = Arc::new(AtomicU64::new(0));
        let clock_now = Arc::clone(&now);
        let slate = slate();
        let live = PassthroughLive::new(
            PassthroughSpec {
                cadence: CADENCE,
                codec: CodecKind::H264,
                stream_params: None,
            },
            reader,
            &slate,
            Box::new(move || clock_now.load(Ordering::Acquire)),
        )
        .unwrap();
        let status = live.status();
        let gate = live.move_gate();
        let sets = live.parameter_sets().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let worker = RenditionWorker::spawn(
            0,
            CADENCE,
            live.time_base(),
            Box::new(Feeding {
                tap,
                script,
                now,
                live,
                gate,
                sets,
                log: Arc::clone(&log),
            }),
            move |e| {
                if let DispatchEvent::Video {
                    tick, watermark, ..
                } = e
                {
                    sink.lock()
                        .unwrap()
                        .push((tick, watermark.map(|w| w.complete_before_ns)));
                }
                Ok(())
            },
        )
        .unwrap();
        let canvas = Arc::new(
            Nv12Image::solid(
                2,
                2,
                16,
                128,
                128,
                multiview_core::color::ColorInfo::default(),
            )
            .unwrap(),
        );
        for tick in 0..ticks {
            // The worker queue is 4 deep: wait for room instead of dropping
            // (GapFill's fill path is not under test here).
            loop {
                match worker.offer(BakedTick {
                    tick,
                    canvas: Arc::clone(&canvas),
                }) {
                    Ok(()) => break,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
                }
            }
        }
        worker.join().unwrap();
        let log = log.lock().unwrap().clone();
        let events = events.lock().unwrap().clone();
        (log, events, status)
    }

    fn scenario() -> Outcome {
        run(
            700,
            Script {
                stall: 100..110,
                change: 210,
            },
        )
    }

    /// One AU per tick across start, run, stall, return and a parameter
    /// change: DTS = tick for every packet; PTS - DTS = the source reorder
    /// plus the clock's bounded extra on source AUs, bounded on the slate.
    #[test]
    fn one_correctly_stamped_packet_per_tick_across_run_stall_return_and_param_change() {
        let (log, _, status) = scenario();
        assert_eq!(log.len(), 700);
        for (i, r) in log.iter().enumerate() {
            assert_eq!(r.count, 1, "tick {}: exactly one packet", r.tick);
            assert_eq!(r.tick, u64::try_from(i).unwrap());
            assert_eq!(r.dts, i64::try_from(r.tick).unwrap(), "DTS = tick");
            let d = r.pts - r.dts;
            match r.state {
                PassthroughState::Passthrough => assert!(
                    (REORDER..=REORDER + 8).contains(&d),
                    "tick {}: reorder {d}",
                    r.tick
                ),
                PassthroughState::Slate | PassthroughState::Repeat => {
                    assert!((0..=8).contains(&d), "tick {}: delay {d}", r.tick);
                }
                PassthroughState::Idle => panic!("idle after a tick"),
            }
        }
        // Passthrough bytes equal the source AU's bytes.
        let first_au = log
            .iter()
            .find(|r| r.state == PassthroughState::Passthrough)
            .unwrap();
        assert_eq!(first_au.data, idr_au(SPS_A, 0));
        // Started on the slate, passed through, stalled, returned, changed
        // parameters, returned: three runs.
        assert!(status.runs.load(Ordering::Relaxed) >= 3);
        assert_eq!(status.stalls.load(Ordering::Relaxed), 1);
        assert_eq!(status.param_changes.load(Ordering::Relaxed), 1);
        let pass = |a: u64, b: u64| {
            log[usize::try_from(a).unwrap()..usize::try_from(b).unwrap()]
                .iter()
                .all(|r| r.state == PassthroughState::Passthrough)
        };
        assert!(pass(10, 100), "run before the stall");
        assert!(
            log[104].state == PassthroughState::Slate,
            "stall bridges on its tick"
        );
        assert!(
            log[690].state == PassthroughState::Passthrough,
            "returned after the change"
        );
        // Return after the stall waits >= 30 on-time ticks.
        let back = log
            .iter()
            .skip(104)
            .find(|r| r.state == PassthroughState::Passthrough)
            .unwrap();
        assert!(back.tick >= 110 + 30, "returned at {}", back.tick);
        // Return after the parameter change waits >= 300 ticks.
        let bridged = log
            .iter()
            .skip(200)
            .find(|r| r.state == PassthroughState::Slate)
            .unwrap();
        let back = log
            .iter()
            .skip(usize::try_from(bridged.tick).unwrap())
            .find(|r| r.state == PassthroughState::Passthrough)
            .unwrap();
        assert!(back.tick >= bridged.tick + 300, "returned at {}", back.tick);
        assert_eq!(back.data[..4 + SPS_B.len()][4..], *SPS_B, "new sets");
    }

    /// The move gate passes only a run-start strict IDR or the slate's IDR;
    /// the slate is always entered at its IDR.
    #[test]
    fn the_move_gate_passes_only_run_start_idrs_and_slate_idrs() {
        let (log, _, _) = scenario();
        let slate_idr = idr_au(SPS_SLATE, 0xEE);
        let mut prev = PassthroughState::Idle;
        let mut mid_run_idrs = 0;
        for r in &log {
            let run_start = r.state == PassthroughState::Passthrough && prev != r.state && r.key;
            let is_slate_idr = r.state == PassthroughState::Slate && r.data == slate_idr;
            assert_eq!(r.gate, run_start || is_slate_idr, "tick {}: {r:?}", r.tick);
            if r.state == PassthroughState::Passthrough && prev == r.state && r.key {
                mid_run_idrs += 1;
            }
            if r.state == PassthroughState::Slate && prev != PassthroughState::Slate {
                assert_eq!(
                    r.data, slate_idr,
                    "tick {}: slate entered at its IDR",
                    r.tick
                );
                assert!(r.key);
            }
            if r.state == PassthroughState::Passthrough && prev != r.state {
                assert!(r.key, "tick {}: a run starts at a strict IDR", r.tick);
            }
            prev = r.state;
        }
        assert!(
            mid_run_idrs > 5,
            "mid-run IDRs keep the key flag, gate false"
        );
    }

    /// Parameter sets: the slate's and each source generation's differ, so a
    /// move or an internal switch with different sets is flagged.
    #[test]
    fn parameter_sets_name_the_slate_and_each_source_generation() {
        let (log, _, _) = scenario();
        let sets_where = |f: &dyn Fn(&Rec) -> bool| -> Vec<RapParameterSets> {
            let mut v: Vec<RapParameterSets> = Vec::new();
            for r in log.iter().filter(|r| r.key && f(r)) {
                if !v.contains(&r.sets) {
                    v.push(r.sets.clone());
                }
            }
            v
        };
        let slate = sets_where(&|r| r.state == PassthroughState::Slate);
        let source = sets_where(&|r| r.state == PassthroughState::Passthrough);
        assert_eq!(slate.len(), 1);
        assert_eq!(source.len(), 2, "generations A and B");
        assert!(!source.contains(&slate[0]));
        for r in log.iter().filter(|r| !r.key) {
            assert_eq!(r.sets, RapParameterSets::Absent);
        }
    }

    /// The watermark is exact and never above any later packet's DTS.
    #[test]
    fn the_watermark_is_never_above_a_later_packets_dts() {
        let (log, events, _) = scenario();
        assert_eq!(events.len(), log.len());
        for (i, (tick, mark)) in events.iter().enumerate() {
            let mark = mark.expect("a DtsIsTick key always proves its window");
            let exact = next_tick_lower_bound_ns(*tick, CADENCE)
                .unwrap()
                .complete_before_ns;
            assert_eq!(mark, exact);
            if let Some(next) = log.get(i + 1) {
                let next_ns = i128::from(next.dts) * 1_000_000_000 / 25;
                assert!(mark <= next_ns, "tick {tick}");
            }
        }
    }

    /// A tap that never sends: the key emits the slate on every tick (never
    /// a hole) and never blocks.
    #[test]
    fn a_silent_tap_yields_the_slate_every_tick() {
        let (log, _, status) = run(
            60,
            Script {
                stall: 0..u64::MAX,
                change: u64::MAX,
            },
        );
        assert_eq!(log.len(), 60);
        assert!(log
            .iter()
            .all(|r| r.state == PassthroughState::Slate && r.count == 1));
        assert_eq!(log[0].data, idr_au(SPS_SLATE, 0xEE));
        assert_eq!(status.slate_ticks.load(Ordering::Relaxed), 60);
        assert_eq!(status.reason(), Some(BridgeReason::AwaitingIdr));
    }

    /// The slate bake uses the slot's own encoder once and keeps one closed
    /// GOP led by a self-contained IDR.
    #[test]
    fn the_format_slate_bakes_one_closed_gop_with_the_slot_encoder() {
        let cfg = multiview_output::sink::EncodeConfig {
            codec_name: "libx264".to_owned(),
            width: 64,
            height: 64,
            format: ffmpeg_next::format::Pixel::YUV420P,
            cadence: CADENCE,
            gop: 10,
            bit_rate: 100_000,
            audio: None,
            cuda_ordinal: None,
        };
        let black = Nv12Image::solid(
            64,
            64,
            16,
            128,
            128,
            multiview_core::color::ColorInfo::default(),
        )
        .unwrap();
        let slate = bake_slate(&cfg, &black, CodecKind::H264).unwrap();
        assert_eq!(slate.packets().len(), 10);
        assert!(idr::is_self_contained_idr(
            &slate.packets()[0],
            CodecKind::H264,
            NalFraming::AnnexB
        ));
        assert!(slate.packets()[1..].iter().all(|p| !idr::is_idr(
            p,
            CodecKind::H264,
            NalFraming::AnnexB
        )));
    }
}
