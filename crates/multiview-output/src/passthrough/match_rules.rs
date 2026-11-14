//! Passthrough match rules (output-routing §12.5): does a source's coded stream
//! fit an output slot so the slot can pass its access units through untouched?
//!
//! [`evaluate`] combines three inputs:
//! * [`SourceTraits`] — the ingest probe's static description of the stream;
//! * [`StreamMeter`] — measurements over the last 10 s of access units
//!   (constant frame rate, GOP, bitrate, B-frames, strict RAPs), fed on the
//!   clock-reader side in O(1) per AU with bounded memory;
//! * [`SlotSpec`] — what the slot's medium and settings allow.
//!
//! It returns the **first** miss as a [`MatchMiss`]. Static trait checks come
//! first (codec, profile, level, size, rate, scan, colour, depth/chroma), then
//! the measured ones (history, CFR, GOP, strict RAP, bitrate, B-frames), so a
//! source that can never match reports the permanent reason rather than
//! "not enough history".
//!
//! All arithmetic is exact integer/rational (i128 cross-multiplication); no
//! float frame rates (invariant #3).

use std::collections::VecDeque;

use multiview_core::time::Rational;

/// Measurement window: 10 s.
pub const METER_WINDOW_SECS: i64 = 10;

/// Hard cap on the meter ring (10 s at 60 fps is 600 entries).
pub const METER_CAPACITY: usize = 1024;

/// Number of recent IDR-to-IDR intervals remembered for the GOP measurement.
const GOP_HISTORY: usize = 16;

/// Coded video codec, as far as passthrough matching cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassCodec {
    /// H.264 / AVC.
    H264,
    /// H.265 / HEVC.
    Hevc,
    /// AV1.
    Av1,
    /// Anything else (never passthrough-capable in slice D).
    Other,
}

/// Scan type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    /// Progressive frames.
    Progressive,
    /// Interlaced (field-coded or MBAFF/PAFF).
    Interlaced,
}

/// The four colour tags (ISO/IEC 23091-2 code points). `None` = untagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColourTags {
    /// `colour_primaries`.
    pub primaries: Option<u8>,
    /// `transfer_characteristics`.
    pub transfer: Option<u8>,
    /// `matrix_coefficients`.
    pub matrix: Option<u8>,
    /// Range: `Some(0)` limited, `Some(1)` full.
    pub range: Option<u8>,
}

impl ColourTags {
    /// Equal only when all four tags are present on both sides and equal;
    /// untagged is never equal (not even to untagged).
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        fn eq(a: Option<u8>, b: Option<u8>) -> bool {
            matches!((a, b), (Some(x), Some(y)) if x == y)
        }
        eq(self.primaries, other.primaries)
            && eq(self.transfer, other.transfer)
            && eq(self.matrix, other.matrix)
            && eq(self.range, other.range)
    }
}

/// The ingest probe's description of a source's video stream.
#[derive(Debug, Clone)]
pub struct SourceTraits {
    /// Codec.
    pub codec: PassCodec,
    /// `profile_idc` (H.264) / `general_profile_idc` (HEVC).
    pub profile: u8,
    /// `level_idc` (H.264) / `general_level_idc` (HEVC).
    pub level: u8,
    /// Coded width in pixels (after cropping).
    pub width: u32,
    /// Coded height in pixels (after cropping).
    pub height: u32,
    /// Nominal frame rate (exact rational).
    pub frame_rate: Rational,
    /// Scan.
    pub scan: Scan,
    /// Colour tags.
    pub colour: ColourTags,
    /// Luma bit depth.
    pub bit_depth: u8,
    /// `chroma_format_idc` (1 = 4:2:0).
    pub chroma: u8,
}

/// What an output slot accepts for passthrough.
#[derive(Debug, Clone)]
pub struct SlotSpec {
    /// Codec.
    pub codec: PassCodec,
    /// Highest accepted profile indicator (numeric compare; for H.264 the
    /// common ladder 66 < 77 < 100 < 110 < 122 < 244 is monotonic in tools).
    pub max_profile: u8,
    /// Highest accepted level indicator.
    pub max_level: u8,
    /// Slot width.
    pub width: u32,
    /// Slot height.
    pub height: u32,
    /// Output frame rate (exact rational).
    pub fps: Rational,
    /// Slot scan.
    pub scan: Scan,
    /// Slot colour tags.
    pub colour: ColourTags,
    /// Slot bit depth.
    pub bit_depth: u8,
    /// Slot `chroma_format_idc`.
    pub chroma: u8,
    /// Bitrate ceiling in kbit/s.
    pub bitrate_ceiling_kbps: u64,
    /// Longest IDR-to-IDR distance the medium allows, in frames (ticks); for
    /// HLS that is `segment_ms` in ticks.
    pub max_gop_ticks: u64,
    /// Whether the medium accepts B-frames (WebRTC does not).
    pub b_frames_allowed: bool,
}

/// The first rule a source misses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMiss {
    /// A DTS delta in the window is not exactly one frame period.
    NotCfr,
    /// Different codec.
    CodecDiffers,
    /// Profile above the slot's.
    ProfileAbove,
    /// Level above the slot's.
    LevelAbove,
    /// Coded size differs.
    SizeDiffers,
    /// Frame rate differs (as an exact rational).
    RateDiffers,
    /// Scan differs.
    ScanDiffers,
    /// A colour tag differs or is untagged.
    ColourDiffers,
    /// Bit depth or chroma format differs.
    DepthOrChroma,
    /// Measured GOP longer than the medium allows.
    GopTooLong,
    /// Measured bitrate above the slot's ceiling.
    BitrateAbove,
    /// B-frames where the medium forbids them.
    BFramesNotAllowed,
    /// No strict random-access point (self-contained IDR) in the window.
    NoStrictRap,
    /// Not enough measured history yet (window under 10 s, or fewer than two
    /// strict IDRs seen).
    NotEnoughHistory,
}

impl MatchMiss {
    /// Kebab-case reason for the readback.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotCfr => "not-cfr",
            Self::CodecDiffers => "codec-differs",
            Self::ProfileAbove => "profile-above",
            Self::LevelAbove => "level-above",
            Self::SizeDiffers => "size-differs",
            Self::RateDiffers => "rate-differs",
            Self::ScanDiffers => "scan-differs",
            Self::ColourDiffers => "colour-differs",
            Self::DepthOrChroma => "depth-or-chroma",
            Self::GopTooLong => "gop-too-long",
            Self::BitrateAbove => "bitrate-above",
            Self::BFramesNotAllowed => "b-frames-not-allowed",
            Self::NoStrictRap => "no-strict-rap",
            Self::NotEnoughHistory => "not-enough-history",
        }
    }
}

impl std::fmt::Display for MatchMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One metered access unit.
#[derive(Debug, Clone, Copy)]
struct Sample {
    dts: i64,
    pts: i64,
    size: usize,
    strict_idr: bool,
}

/// Rolling measurements over the last 10 s of a source's access units.
///
/// Bounded: at most [`METER_CAPACITY`] samples plus [`GOP_HISTORY`] GOP
/// intervals. [`StreamMeter::push`] is amortised O(1).
#[derive(Debug, Clone, Default)]
pub struct StreamMeter {
    ring: VecDeque<Sample>,
    time_base: Option<Rational>,
    /// AUs seen since the most recent strict IDR (or since the meter started,
    /// when none was seen yet).
    since_idr: u64,
    /// Whether any strict IDR has been seen.
    seen_idr: bool,
    /// Recent IDR-to-IDR distances in AUs.
    gops: VecDeque<u64>,
    /// A non-monotonic DTS or a time-base change was seen in the window; the
    /// meter restarts on it.
    restarts: u64,
}

impl StreamMeter {
    /// An empty meter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Times the meter restarted (time-base change or non-monotonic DTS).
    #[must_use]
    pub fn restarts(&self) -> u64 {
        self.restarts
    }

    /// Feed one access unit.
    pub fn push(&mut self, dts: i64, pts: i64, size: usize, strict_idr: bool, time_base: Rational) {
        let tb_changed = self.time_base.is_some_and(|tb| tb != time_base);
        let backwards = self.ring.back().is_some_and(|s| dts <= s.dts);
        if tb_changed || backwards || !valid_tb(time_base) {
            self.reset();
            self.restarts = self.restarts.saturating_add(1);
            if !valid_tb(time_base) {
                return;
            }
        }
        self.time_base = Some(time_base);

        if strict_idr {
            if self.seen_idr {
                if self.gops.len() == GOP_HISTORY {
                    self.gops.pop_front();
                }
                self.gops.push_back(self.since_idr);
            }
            self.seen_idr = true;
            self.since_idr = 0;
        }
        self.since_idr = self.since_idr.saturating_add(1);

        self.ring.push_back(Sample {
            dts,
            pts,
            size,
            strict_idr,
        });
        // Keep exactly one sample at or before the window start so the window
        // can be shown to cover the full 10 s.
        while self.ring.len() >= 2 {
            let second = self.ring.get(1).map(|s| s.dts);
            match second {
                Some(d) if covers(time_base, d, dts) => {
                    self.ring.pop_front();
                }
                _ => break,
            }
        }
        while self.ring.len() > METER_CAPACITY {
            self.ring.pop_front();
        }
    }

    fn reset(&mut self) {
        self.ring.clear();
        self.time_base = None;
        self.since_idr = 0;
        self.seen_idr = false;
        self.gops.clear();
    }

    /// Whether the window spans at least 10 s.
    #[must_use]
    pub fn window_full(&self) -> bool {
        match (self.time_base, self.ring.front(), self.ring.back()) {
            (Some(tb), Some(a), Some(b)) => covers(tb, a.dts, b.dts),
            _ => false,
        }
    }

    /// Whether every DTS delta in the window is exactly one frame period at
    /// `rate`. `None` while the window is shorter than 10 s.
    #[must_use]
    pub fn is_cfr(&self, rate: Rational) -> Option<bool> {
        if !self.window_full() {
            return None;
        }
        let tb = self.time_base?;
        if !valid_tb(rate) {
            return Some(false);
        }
        // delta * tb == 1 / rate  <=>  delta * tb.num * rate.num == tb.den * rate.den
        let rhs = i128::from(tb.den) * i128::from(rate.den);
        let k = i128::from(tb.num) * i128::from(rate.num);
        let ok = self
            .ring
            .iter()
            .zip(self.ring.iter().skip(1))
            .all(|(a, b)| (i128::from(b.dts) - i128::from(a.dts)) * k == rhs);
        Some(ok)
    }

    /// Longest recent IDR-to-IDR distance in AUs; `None` before two strict IDRs.
    #[must_use]
    pub fn max_gop(&self) -> Option<u64> {
        self.gops.iter().copied().max()
    }

    /// AUs since the last strict IDR (or since the meter started).
    #[must_use]
    pub fn since_last_idr(&self) -> u64 {
        self.since_idr
    }

    /// Whether the window holds a strict IDR.
    #[must_use]
    pub fn has_strict_rap(&self) -> bool {
        self.ring.iter().any(|s| s.strict_idr)
    }

    /// Bitrate over the window in bit/s (integer); `None` while the window is
    /// shorter than 10 s. The oldest sample only marks the window start, so
    /// its bytes are not counted.
    #[must_use]
    pub fn bitrate_bps(&self) -> Option<u64> {
        if !self.window_full() {
            return None;
        }
        let tb = self.time_base?;
        let first = self.ring.front()?;
        let last = self.ring.back()?;
        let span = i128::from(last.dts) - i128::from(first.dts);
        let bytes: i128 = self
            .ring
            .iter()
            .skip(1)
            .map(|s| i128::try_from(s.size).unwrap_or(i128::MAX / 16))
            .sum();
        // bits / (span * tb.num / tb.den)
        let denom = span * i128::from(tb.num);
        if denom <= 0 {
            return None;
        }
        let bps = bytes.saturating_mul(8).saturating_mul(i128::from(tb.den)) / denom;
        Some(u64::try_from(bps).unwrap_or(u64::MAX))
    }

    /// Whether the window shows B-frames: a `pts - dts` different from the
    /// first sample's, or a PTS going backwards in decode order.
    #[must_use]
    pub fn has_b_frames(&self) -> bool {
        let Some(first) = self.ring.front() else {
            return false;
        };
        let base = i128::from(first.pts) - i128::from(first.dts);
        let reorder = self
            .ring
            .iter()
            .any(|s| i128::from(s.pts) - i128::from(s.dts) != base);
        let backwards = self
            .ring
            .iter()
            .zip(self.ring.iter().skip(1))
            .any(|(a, b)| b.pts <= a.pts);
        reorder || backwards
    }
}

fn valid_tb(r: Rational) -> bool {
    r.num > 0 && r.den > 0
}

/// Whether `[from, to]` spans at least the window in time base `tb`.
fn covers(tb: Rational, from: i64, to: i64) -> bool {
    // (to - from) * tb.num / tb.den >= WINDOW
    let span = i128::from(to) - i128::from(from);
    span * i128::from(tb.num) >= i128::from(METER_WINDOW_SECS) * i128::from(tb.den)
}

/// Evaluate the match rules; `Ok(())` when the source can be passed through.
///
/// # Errors
/// The first [`MatchMiss`] (static traits first, then measurements).
pub fn evaluate(
    traits: &SourceTraits,
    meter: &StreamMeter,
    slot: &SlotSpec,
) -> Result<(), MatchMiss> {
    if traits.codec != slot.codec {
        return Err(MatchMiss::CodecDiffers);
    }
    if traits.profile > slot.max_profile {
        return Err(MatchMiss::ProfileAbove);
    }
    if traits.level > slot.max_level {
        return Err(MatchMiss::LevelAbove);
    }
    if traits.width != slot.width || traits.height != slot.height {
        return Err(MatchMiss::SizeDiffers);
    }
    if !valid_tb(traits.frame_rate) || !valid_tb(slot.fps) || traits.frame_rate != slot.fps {
        return Err(MatchMiss::RateDiffers);
    }
    if traits.scan != slot.scan {
        return Err(MatchMiss::ScanDiffers);
    }
    if !traits.colour.matches(&slot.colour) {
        return Err(MatchMiss::ColourDiffers);
    }
    if traits.bit_depth != slot.bit_depth || traits.chroma != slot.chroma {
        return Err(MatchMiss::DepthOrChroma);
    }
    // A GOP already longer than allowed is decisive with any history.
    if meter.since_last_idr() > slot.max_gop_ticks {
        return Err(MatchMiss::GopTooLong);
    }
    match meter.is_cfr(slot.fps) {
        None => return Err(MatchMiss::NotEnoughHistory),
        Some(false) => return Err(MatchMiss::NotCfr),
        Some(true) => {}
    }
    if !meter.has_strict_rap() {
        return Err(MatchMiss::NoStrictRap);
    }
    match meter.max_gop() {
        None => return Err(MatchMiss::NotEnoughHistory),
        Some(g) if g > slot.max_gop_ticks => return Err(MatchMiss::GopTooLong),
        Some(_) => {}
    }
    match meter.bitrate_bps() {
        None => return Err(MatchMiss::NotEnoughHistory),
        Some(bps) if bps > slot.bitrate_ceiling_kbps.saturating_mul(1000) => {
            return Err(MatchMiss::BitrateAbove);
        }
        Some(_) => {}
    }
    if !slot.b_frames_allowed && meter.has_b_frames() {
        return Err(MatchMiss::BFramesNotAllowed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TB: Rational = Rational::new(1, 90_000);
    const FPS: Rational = Rational::FPS_29_97;
    const PERIOD: i64 = 3003;

    fn tags() -> ColourTags {
        ColourTags {
            primaries: Some(1),
            transfer: Some(1),
            matrix: Some(1),
            range: Some(0),
        }
    }

    fn traits() -> SourceTraits {
        SourceTraits {
            codec: PassCodec::H264,
            profile: 100,
            level: 40,
            width: 1920,
            height: 1080,
            frame_rate: FPS,
            scan: Scan::Progressive,
            colour: tags(),
            bit_depth: 8,
            chroma: 1,
        }
    }

    fn slot() -> SlotSpec {
        SlotSpec {
            codec: PassCodec::H264,
            max_profile: 100,
            max_level: 41,
            width: 1920,
            height: 1080,
            fps: FPS,
            scan: Scan::Progressive,
            colour: tags(),
            bit_depth: 8,
            chroma: 1,
            bitrate_ceiling_kbps: 8000,
            max_gop_ticks: 120,
            b_frames_allowed: true,
        }
    }

    /// A metered P-only source: `n` AUs, IDR every `gop`, `size` bytes each.
    fn meter(n: i64, gop: i64, size: usize) -> StreamMeter {
        let mut m = StreamMeter::new();
        for i in 0..n {
            m.push(i * PERIOD, i * PERIOD, size, i % gop == 0, TB);
        }
        m
    }

    #[test]
    #[allow(clippy::too_many_lines, clippy::items_after_statements)]
    fn table() {
        // 25 kB per frame at 29.97 fps is about 5.99 Mbit/s.
        let good = meter(400, 60, 25_000);
        type Case = (
            &'static str,
            Box<dyn Fn(&mut SourceTraits, &mut SlotSpec)>,
            Option<MatchMiss>,
        );
        let cases: Vec<Case> = vec![
            ("all match", Box::new(|_, _| {}), None),
            (
                "codec",
                Box::new(|t, _| t.codec = PassCodec::Hevc),
                Some(MatchMiss::CodecDiffers),
            ),
            (
                "profile",
                Box::new(|t, _| t.profile = 110),
                Some(MatchMiss::ProfileAbove),
            ),
            ("profile below ok", Box::new(|t, _| t.profile = 77), None),
            (
                "level",
                Box::new(|t, _| t.level = 42),
                Some(MatchMiss::LevelAbove),
            ),
            (
                "size",
                Box::new(|t, _| t.height = 1088),
                Some(MatchMiss::SizeDiffers),
            ),
            (
                "rate",
                Box::new(|t, _| t.frame_rate = Rational::FPS_30),
                Some(MatchMiss::RateDiffers),
            ),
            (
                "rate equal by value",
                Box::new(|t, _| t.frame_rate = Rational::new(60_000, 2002)),
                None,
            ),
            (
                "scan",
                Box::new(|t, _| t.scan = Scan::Interlaced),
                Some(MatchMiss::ScanDiffers),
            ),
            (
                "colour",
                Box::new(|t, _| t.colour.matrix = Some(6)),
                Some(MatchMiss::ColourDiffers),
            ),
            (
                "untagged",
                Box::new(|t, _| t.colour.range = None),
                Some(MatchMiss::ColourDiffers),
            ),
            (
                "untagged both",
                Box::new(|t, s| {
                    t.colour.range = None;
                    s.colour.range = None;
                }),
                Some(MatchMiss::ColourDiffers),
            ),
            (
                "depth",
                Box::new(|t, _| t.bit_depth = 10),
                Some(MatchMiss::DepthOrChroma),
            ),
            (
                "chroma",
                Box::new(|t, _| t.chroma = 2),
                Some(MatchMiss::DepthOrChroma),
            ),
            (
                "gop",
                Box::new(|_, s| s.max_gop_ticks = 59),
                Some(MatchMiss::GopTooLong),
            ),
            (
                "bitrate",
                Box::new(|_, s| s.bitrate_ceiling_kbps = 5000),
                Some(MatchMiss::BitrateAbove),
            ),
            (
                "bitrate at ceiling ok",
                Box::new(|_, s| s.bitrate_ceiling_kbps = 6000),
                None,
            ),
            // The first miss wins: codec before size.
            (
                "first miss",
                Box::new(|t, _| {
                    t.codec = PassCodec::Av1;
                    t.width = 1280;
                }),
                Some(MatchMiss::CodecDiffers),
            ),
        ];
        for (name, mutate, want) in cases {
            let mut t = traits();
            let mut s = slot();
            mutate(&mut t, &mut s);
            assert_eq!(evaluate(&t, &good, &s).err(), want, "{name}");
        }
    }

    #[test]
    fn needs_ten_seconds_and_two_idrs() {
        // 299 frames at 29.97 is 9.977 s: short.
        assert_eq!(
            evaluate(&traits(), &meter(300, 60, 1000), &slot()).err(),
            Some(MatchMiss::NotEnoughHistory)
        );
        // 301 frames span 300 periods = 10.01 s.
        assert_eq!(
            evaluate(&traits(), &meter(301, 60, 1000), &slot()).err(),
            None
        );
        // One strict IDR only (inside the window), GOP limit generous: the
        // GOP cannot be measured yet.
        let mut s = slot();
        s.max_gop_ticks = 1000;
        let mut m = StreamMeter::new();
        for i in 0..400 {
            m.push(i * PERIOD, i * PERIOD, 1000, i == 150, TB);
        }
        assert_eq!(
            evaluate(&traits(), &m, &s).err(),
            Some(MatchMiss::NotEnoughHistory)
        );
    }

    #[test]
    fn running_gop_beyond_limit_is_decisive_early() {
        let m = meter(130, 1000, 1000);
        assert_eq!(
            evaluate(&traits(), &m, &slot()).err(),
            Some(MatchMiss::GopTooLong)
        );
    }

    #[test]
    fn vfr_or_dropped_frame_is_not_cfr() {
        let mut m = StreamMeter::new();
        for i in 0..400 {
            let skip = if i >= 200 { PERIOD } else { 0 };
            m.push(i * PERIOD + skip, i * PERIOD + skip, 1000, i % 60 == 0, TB);
        }
        assert_eq!(
            evaluate(&traits(), &m, &slot()).err(),
            Some(MatchMiss::NotCfr)
        );
        // The dropped frame leaves the window after 10 s.
        for i in 400..800 {
            m.push(
                i * PERIOD + PERIOD,
                i * PERIOD + PERIOD,
                1000,
                i % 60 == 0,
                TB,
            );
        }
        assert_eq!(evaluate(&traits(), &m, &slot()).err(), None);
    }

    #[test]
    fn no_strict_rap_in_window() {
        let mut m = StreamMeter::new();
        for i in 0..400 {
            m.push(i * PERIOD, i * PERIOD, 1000, false, TB);
        }
        let mut s = slot();
        s.max_gop_ticks = 10_000;
        assert_eq!(
            evaluate(&traits(), &m, &s).err(),
            Some(MatchMiss::NoStrictRap)
        );
    }

    #[test]
    fn b_frames_detected_and_refused_where_disallowed() {
        // IBBP closed GOP of 12, reorder delay one frame.
        let disp = [0_i64, 3, 1, 2, 6, 4, 5, 9, 7, 8, 11, 10];
        let mut m = StreamMeter::new();
        for i in 0..480_i64 {
            let gop = i / 12;
            let k = usize::try_from(i % 12).unwrap();
            let pts = (gop * 12 + disp[k]) * PERIOD;
            m.push((i - 1) * PERIOD, pts, 1000, k == 0, TB);
        }
        assert!(m.has_b_frames());
        assert_eq!(evaluate(&traits(), &m, &slot()).err(), None);
        let mut s = slot();
        s.b_frames_allowed = false;
        assert_eq!(
            evaluate(&traits(), &m, &s).err(),
            Some(MatchMiss::BFramesNotAllowed)
        );
        // A P-only source with a constant non-zero pts - dts is not B-frames.
        let mut p = StreamMeter::new();
        for i in 0..400 {
            p.push(i * PERIOD, (i + 2) * PERIOD, 1000, i % 60 == 0, TB);
        }
        assert!(!p.has_b_frames());
    }

    #[test]
    fn meter_memory_is_bounded() {
        let mut m = StreamMeter::new();
        // 240 fps for 20 s: 10 s would be 2400 samples; the ring caps at 1024.
        for i in 0..4800 {
            m.push(i * 375, i * 375, 10, i % 240 == 0, TB);
        }
        assert!(m.ring.len() <= METER_CAPACITY);
        assert!(m.gops.len() <= GOP_HISTORY);
    }

    #[test]
    fn backwards_dts_restarts_meter() {
        let mut m = meter(400, 60, 1000);
        m.push(0, 0, 1000, true, TB);
        assert_eq!(m.restarts(), 1);
        assert!(!m.window_full());
    }

    #[test]
    fn reasons_are_kebab_case() {
        assert_eq!(
            MatchMiss::BFramesNotAllowed.as_str(),
            "b-frames-not-allowed"
        );
        assert_eq!(
            MatchMiss::NotEnoughHistory.to_string(),
            "not-enough-history"
        );
    }
}
