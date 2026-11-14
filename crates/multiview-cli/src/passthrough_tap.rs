//! The ingest side of passthrough (output-routing §12.5, issue #344 D2a): one
//! [`SourceTap`] per source, found through a wait-free registry, and the
//! per-connection [`TapWriter`] the demux reader thread feeds.
//!
//! ## Cost and blocking (invariants #1 / #10)
//!
//! * Without a consumer the writer does one atomic load per packet
//!   ([`multiview_output::passthrough::CompressedTap::has_consumers`]) and
//!   returns: no copy, no classification, no allocation.
//! * With a consumer it copies the packet bytes once into an `Arc<[u8]>`,
//!   classifies it with pure header parsers and `offer`s it; `offer` only
//!   `try_lock`s / `try_send`s (D1), so the reader thread never waits.
//! * The registry is a [`SwapCell`] of an immutable map: a lookup is one
//!   wait-free load; inserts and removals are control-plane `rcu`s.
//!
//! ## Parameter generations and connections
//!
//! `params_gen` (per source, monotonic across reconnects) is bumped when a
//! keyframe's in-band parameter sets differ from the last seen ones (a
//! rejected scan is a change), and on every (re)connect: a reconnect is a
//! parameter change. The stream's [`SourceTraits`] are published per
//! connection with a connection generation ([`TapStream::connection`]) and
//! refreshed from the first in-band SPS when the container's extradata had
//! none (MPEG-TS often has none at open).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use multiview_core::time::Rational;
use multiview_ffmpeg::idr::{self, CodecKind, NalFraming, ParameterSetScan};
use multiview_framestore::SwapCell;
use multiview_output::passthrough::match_rules::{ColourTags, PassCodec, Scan};
use multiview_output::passthrough::{CompressedTap, SourceTraits, TapAu, MAX_AU_BYTES};

/// The stream description a tap publishes per connection.
#[derive(Debug, Clone)]
pub struct TapStream {
    /// The connection generation (bumped on every (re)connect of the source).
    pub connection: u64,
    /// The stream's codec.
    pub codec: CodecKind,
    /// The NAL framing of the stream's access units.
    pub framing: NalFraming,
    /// The stream time base.
    pub time_base: Rational,
    /// The match-rule traits (output-routing §12.5).
    pub traits: SourceTraits,
}

/// One source's compressed tap and stream description.
pub struct SourceTap {
    tap: CompressedTap,
    stream: SwapCell<Option<Arc<TapStream>>>,
    connections: AtomicU64,
    params_gen: AtomicU64,
    /// Packets skipped before the copy because they exceed `MAX_AU_BYTES`.
    oversize: AtomicU64,
    /// Packets skipped because they carried no DTS/PTS or no run clock existed.
    unstamped: AtomicU64,
}

impl std::fmt::Debug for SourceTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceTap")
            .field("tap", &self.tap)
            .field("connection", &self.connections.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Default for SourceTap {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceTap {
    /// A tap with no consumer and no connection yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tap: CompressedTap::new(),
            stream: SwapCell::empty(),
            connections: AtomicU64::new(0),
            params_gen: AtomicU64::new(0),
            oversize: AtomicU64::new(0),
            unstamped: AtomicU64::new(0),
        }
    }

    /// The compressed tap (subscribe here for a reader).
    #[must_use]
    pub fn tap(&self) -> &CompressedTap {
        &self.tap
    }

    /// The current connection's stream description (wait-free).
    #[must_use]
    pub fn stream(&self) -> Option<Arc<TapStream>> {
        self.stream.load()
    }

    /// The current parameter generation.
    #[must_use]
    pub fn params_gen(&self) -> u64 {
        self.params_gen.load(Ordering::Acquire)
    }

    /// Packets the writer skipped as oversize (never copied).
    #[must_use]
    pub fn oversize(&self) -> u64 {
        self.oversize.load(Ordering::Relaxed)
    }

    /// Packets the writer skipped for a missing timestamp or run clock.
    #[must_use]
    pub fn unstamped(&self) -> u64 {
        self.unstamped.load(Ordering::Relaxed)
    }

    fn bump_gen(&self) -> u64 {
        self.params_gen
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }
}

/// The process-wide registry of source taps: an immutable map behind a
/// wait-free cell (like the routing store lookup).
type TapMap = SwapCell<Arc<HashMap<String, Arc<SourceTap>>>>;

fn registry() -> &'static TapMap {
    static TAPS: OnceLock<TapMap> = OnceLock::new();
    TAPS.get_or_init(|| SwapCell::from_pointee(HashMap::new()))
}

/// The tap of source `id`, created and registered on first use. Control
/// plane only (plan build); a repeated call returns the same `Arc`.
#[must_use]
pub fn source_tap(id: &str) -> Arc<SourceTap> {
    if let Some(tap) = lookup(id) {
        return tap;
    }
    let mut out = None;
    registry().rcu(|current| {
        let mut next = current.as_ref().clone();
        let tap = next
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(SourceTap::new()));
        out = Some(Arc::clone(tap));
        next
    });
    out.unwrap_or_else(|| Arc::new(SourceTap::new()))
}

/// The registered tap of source `id` (one wait-free load; never creates).
#[must_use]
pub fn lookup(id: &str) -> Option<Arc<SourceTap>> {
    registry().load().get(id).cloned()
}

/// Forget source `id`'s tap (source removed). A reader already subscribed
/// keeps its own handle; a re-added source gets a fresh tap.
pub fn forget(id: &str) {
    if registry().load().contains_key(id) {
        registry().rcu(|current| {
            let mut next = current.as_ref().clone();
            next.remove(id);
            next
        });
    }
}

/// The stream facts a connection opens with (from the demuxer, once).
#[derive(Debug, Clone)]
pub struct OpenFacts {
    /// Codec and framing (`idr` classifier terms).
    pub codec: CodecKind,
    /// NAL framing.
    pub framing: NalFraming,
    /// Stream time base.
    pub time_base: Rational,
    /// Declared frame rate (exact rational).
    pub frame_rate: Rational,
    /// libav `profile` (fallback when no SPS is known).
    pub profile: i32,
    /// libav `level` (fallback when no SPS is known).
    pub level: i32,
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// Colour primaries code point (2 = unspecified).
    pub color_primaries: i32,
    /// Transfer characteristics code point (2 = unspecified).
    pub color_trc: i32,
    /// Matrix coefficients code point (2 = unspecified).
    pub color_space: i32,
    /// libav range (0 unspecified, 1 limited, 2 full).
    pub color_range: i32,
    /// libav field order (0 unknown, 1 progressive, 2..=5 interlaced).
    pub field_order: i32,
    /// `bits_per_raw_sample` (0 unknown).
    pub bits_per_raw_sample: i32,
    /// Codec extradata (avcC or Annex-B sets; may be empty).
    pub extradata: Vec<u8>,
}

/// The first SPS NAL of H.264 extradata: an avcC record (first byte 1) or
/// Annex-B parameter sets.
fn extradata_sps(extradata: &[u8]) -> Option<&[u8]> {
    if extradata.first() == Some(&1) {
        // avcC: version, profile, compat, level, lengthSize, numSps, len16, sps.
        let count = extradata.get(5)? & 0x1F;
        if count == 0 {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([*extradata.get(6)?, *extradata.get(7)?]));
        return extradata.get(8..8_usize.checked_add(len)?);
    }
    idr::h264_nal_of_type(extradata, NalFraming::AnnexB, 7)
}

fn code_point(v: i32) -> Option<u8> {
    // 2 = "unspecified" (ISO/IEC 23091-2); 0 and above 255 are not tags either.
    (v != 2 && v > 0).then(|| u8::try_from(v).ok()).flatten()
}

/// The match-rule traits of a connection. H.264 profile, `constraint_set`
/// flags (the SPS byte after `profile_idc`), level, bit depth, chroma format
/// and scan come from `sps` when one is known, else from libav's scalars
/// (`constraint_flags` = 0, depth / chroma 0 = unknown: never matches).
#[must_use]
pub fn source_traits(facts: &OpenFacts, sps: Option<&[u8]>) -> SourceTraits {
    let codec = match facts.codec {
        CodecKind::H264 => PassCodec::H264,
        CodecKind::Hevc => PassCodec::Hevc,
        CodecKind::Av1 => PassCodec::Av1,
        _ => PassCodec::Other,
    };
    let parsed = match facts.codec {
        CodecKind::H264 => sps.and_then(multiview_ffmpeg::h264_repeat::parse_sps),
        _ => None,
    };
    let field_scan = if (2..=5).contains(&facts.field_order) {
        Scan::Interlaced
    } else {
        Scan::Progressive
    };
    let (profile, constraint_flags, level, bit_depth, chroma, scan) = match &parsed {
        Some(s) => (
            s.profile_idc,
            s.constraint_flags,
            s.level_idc,
            u8::try_from(s.bit_depth_luma).unwrap_or(0),
            u8::try_from(s.chroma_format_idc).unwrap_or(0),
            if s.frame_mbs_only {
                Scan::Progressive
            } else {
                Scan::Interlaced
            },
        ),
        None => (
            u8::try_from(facts.profile & 0xFF).unwrap_or(0),
            0,
            u8::try_from(facts.level).unwrap_or(0),
            u8::try_from(facts.bits_per_raw_sample).unwrap_or(0),
            0,
            field_scan,
        ),
    };
    SourceTraits {
        codec,
        profile,
        constraint_flags,
        level,
        width: facts.width,
        height: facts.height,
        frame_rate: facts.frame_rate,
        scan,
        colour: ColourTags {
            primaries: code_point(facts.color_primaries),
            transfer: code_point(facts.color_trc),
            matrix: code_point(facts.color_space),
            range: match facts.color_range {
                1 => Some(0),
                2 => Some(1),
                _ => None,
            },
        },
        bit_depth,
        chroma,
    }
}

/// The owned, comparable parameter sets of an AU (4-byte BE length + NAL),
/// the same encoding `StreamCodecParameters::rap_parameter_sets` uses.
fn owned_sets(au: &[u8], codec: CodecKind, framing: NalFraming) -> Result<Option<Vec<u8>>, ()> {
    match idr::scan_parameter_sets(au, codec, framing) {
        ParameterSetScan::Absent => Ok(None),
        ParameterSetScan::Rejected => Err(()),
        ParameterSetScan::Found(sets) => {
            let mut out = Vec::new();
            for set in sets {
                let len = u32::try_from(set.len()).map_err(|_| ())?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(set);
            }
            Ok(Some(out))
        }
    }
}

/// The per-connection writer, owned by the demux reader closure.
pub struct TapWriter {
    tap: Arc<SourceTap>,
    facts: OpenFacts,
    /// The SPS the published traits were derived from.
    traits_sps: Option<Vec<u8>>,
    last_sets: Option<Vec<u8>>,
    gen: u64,
    connection: u64,
    /// The run's monotonic clock in nanoseconds (`None` = no run seeded).
    now_ns: Box<dyn Fn() -> Option<u64> + Send>,
}

impl std::fmt::Debug for TapWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TapWriter")
            .field("connection", &self.connection)
            .field("gen", &self.gen)
            .finish_non_exhaustive()
    }
}

impl TapWriter {
    /// Start a connection on `tap`: bump the connection generation and the
    /// parameter generation (a reconnect is a parameter change) and publish
    /// the stream's traits.
    #[must_use]
    pub fn connect(
        tap: Arc<SourceTap>,
        facts: OpenFacts,
        now_ns: Box<dyn Fn() -> Option<u64> + Send>,
    ) -> Self {
        let connection = tap
            .connections
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let gen = tap.bump_gen();
        let sps = match facts.codec {
            CodecKind::H264 => extradata_sps(&facts.extradata).map(<[u8]>::to_vec),
            _ => None,
        };
        let mut writer = Self {
            tap,
            facts,
            traits_sps: None,
            last_sets: None,
            gen,
            connection,
            now_ns,
        };
        writer.publish(sps);
        writer
    }

    fn publish(&mut self, sps: Option<Vec<u8>>) {
        let traits = source_traits(&self.facts, sps.as_deref());
        self.traits_sps = sps;
        self.tap.stream.store(Some(Arc::new(TapStream {
            connection: self.connection,
            codec: self.facts.codec,
            framing: self.facts.framing,
            time_base: self.facts.time_base,
            traits,
        })));
    }

    /// The connection generation.
    #[must_use]
    pub const fn connection(&self) -> u64 {
        self.connection
    }

    /// Offer one demuxed video packet. Never blocks; with no consumer this is
    /// one atomic load and nothing else.
    pub fn on_packet(&mut self, data: &[u8], dts: Option<i64>, pts: Option<i64>, key: bool) {
        if !self.tap.tap.has_consumers() {
            return;
        }
        if data.len() > MAX_AU_BYTES {
            self.tap.oversize.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let (Some(dts), Some(arrival_ns)) = (dts.or(pts), (self.now_ns)()) else {
            self.tap.unstamped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let pts = pts.unwrap_or(dts);
        let codec = self.facts.codec;
        let framing = self.facts.framing;
        let strict_idr = idr::is_self_contained_idr(data, codec, framing);
        if key || strict_idr {
            match owned_sets(data, codec, framing) {
                Ok(None) => {}
                Ok(Some(sets)) => {
                    if self.last_sets.as_ref() != Some(&sets) {
                        // The first sets of a connection were already a bump
                        // (the connect itself); every later difference is one.
                        if self.last_sets.is_some() {
                            self.gen = self.tap.bump_gen();
                        }
                        self.last_sets = Some(sets);
                    }
                    if codec == CodecKind::H264 {
                        let sps = idr::h264_nal_of_type(data, framing, 7);
                        if sps.is_some() && sps != self.traits_sps.as_deref() {
                            self.publish(sps.map(<[u8]>::to_vec));
                        }
                    }
                }
                Err(()) => {
                    self.gen = self.tap.bump_gen();
                    self.last_sets = None;
                }
            }
        }
        let reference = match codec {
            CodecKind::H264 => idr::h264_is_reference(data, framing).unwrap_or(true),
            _ => true,
        };
        self.tap.tap.offer(TapAu {
            data: Arc::from(data),
            dts,
            pts,
            time_base: self.facts.time_base,
            strict_idr,
            reference,
            params_gen: self.gen,
            arrival_ns,
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// x264 High 1280x720 (the D1 `h264_repeat` fixtures).
    const SPS: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xb2, 0x00, 0xa0, 0x0b, 0x76, 0x02, 0x20, 0x00, 0x00, 0x03,
        0x00, 0x20, 0x00, 0x00, 0x07, 0x81, 0xe3, 0x06, 0x49,
    ];
    /// x264 Constrained Baseline 320x240.
    const SPS2: &[u8] = &[
        0x67, 0x42, 0xc0, 0x0d, 0xda, 0x05, 0x07, 0xec, 0x04, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40,
        0x00, 0x00, 0x0f, 0x03, 0xc5, 0x0a, 0xa8,
    ];
    const PPS: &[u8] = &[0x68, 0xeb, 0xc3, 0xcb, 0x22, 0xc0];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [0_u8, 0, 0, 1].into_iter().chain(n.iter().copied()))
            .collect()
    }
    fn idr_au(sps: &[u8]) -> Vec<u8> {
        annexb(&[sps, PPS, &[0x65, 0x88, 0x80]])
    }
    fn p_au(nal_ref: bool) -> Vec<u8> {
        annexb(&[&[if nal_ref { 0x41 } else { 0x01 }, 0x9A, 0x80]])
    }
    fn facts() -> OpenFacts {
        OpenFacts {
            codec: CodecKind::H264,
            framing: NalFraming::AnnexB,
            time_base: Rational::new(1, 90_000),
            frame_rate: Rational::FPS_29_97,
            profile: 100,
            level: 40,
            width: 1920,
            height: 1080,
            color_primaries: 1,
            color_trc: 1,
            color_space: 1,
            color_range: 1,
            field_order: 0,
            bits_per_raw_sample: 8,
            extradata: Vec::new(),
        }
    }
    fn clock() -> Box<dyn Fn() -> Option<u64> + Send> {
        Box::new(|| Some(7))
    }

    #[test]
    fn the_registry_returns_one_tap_per_source_and_forgets_it() {
        let a = source_tap("pt-reg-a");
        assert!(Arc::ptr_eq(&a, &source_tap("pt-reg-a")));
        assert!(Arc::ptr_eq(&a, &lookup("pt-reg-a").unwrap()));
        assert!(lookup("pt-reg-none").is_none(), "lookup never creates");
        forget("pt-reg-a");
        assert!(lookup("pt-reg-a").is_none());
        assert!(!Arc::ptr_eq(&a, &source_tap("pt-reg-a")));
        forget("pt-reg-a");
    }

    /// Zero cost without a consumer: the clock is never read, nothing is
    /// counted, built or queued.
    #[test]
    fn without_a_consumer_the_writer_touches_nothing() {
        let tap = Arc::new(SourceTap::new());
        let read = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&read);
        let mut w = TapWriter::connect(
            Arc::clone(&tap),
            facts(),
            Box::new(move || {
                flag.store(true, Ordering::Relaxed);
                Some(0)
            }),
        );
        let big = vec![0_u8; MAX_AU_BYTES + 1];
        for n in 0..1000 {
            w.on_packet(&p_au(true), Some(n), Some(n), false);
            w.on_packet(&big, Some(n), Some(n), false);
        }
        assert!(!read.load(Ordering::Relaxed), "no clock read");
        assert_eq!((tap.oversize(), tap.unstamped()), (0, 0));
        assert_eq!(tap.tap().dropped(), 0);
        let reader = tap.tap().subscribe().unwrap();
        assert!(reader.try_recv().is_none(), "nothing queued before");
    }

    /// The reader thread never blocks: an unread consumer fills and the
    /// writer drops (counted) in bounded time.
    #[test]
    fn a_wedged_consumer_never_blocks_the_writer() {
        let tap = Arc::new(SourceTap::new());
        let _reader = tap.tap().subscribe().unwrap();
        let mut w = TapWriter::connect(Arc::clone(&tap), facts(), clock());
        let started = std::time::Instant::now();
        for n in 0..10_000 {
            w.on_packet(&p_au(true), Some(n), Some(n), false);
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(tap.tap().dropped() >= 10_000 - 64);
    }

    #[test]
    fn classification_generations_and_traits() {
        let tap = Arc::new(SourceTap::new());
        let reader = tap.tap().subscribe().unwrap();
        let mut w = TapWriter::connect(Arc::clone(&tap), facts(), clock());
        let g0 = tap.params_gen();
        // No SPS known at open: libav scalars, constraint flags 0, chroma 0.
        let s = tap.stream().unwrap();
        assert_eq!((s.traits.profile, s.traits.constraint_flags), (100, 0));
        assert_eq!(s.traits.chroma, 0);
        assert_eq!(s.traits.colour.range, Some(0));
        w.on_packet(&idr_au(SPS), Some(0), Some(3003), true);
        w.on_packet(&p_au(false), Some(3003), Some(6006), false);
        w.on_packet(&idr_au(SPS), Some(6006), Some(9009), true);
        w.on_packet(&idr_au(SPS2), Some(9009), Some(9009), true);
        let got: Vec<TapAu> = reader.drain().collect();
        assert_eq!(got.len(), 4);
        assert!(got[0].strict_idr && got[0].reference);
        assert_eq!((got[0].dts, got[0].pts, got[0].arrival_ns), (0, 3003, 7));
        assert!(!got[1].strict_idr && !got[1].reference);
        assert_eq!(got[0].params_gen, g0, "first sets of a connection: no bump");
        assert_eq!(got[2].params_gen, g0, "equal sets: same generation");
        assert_eq!(got[3].params_gen, g0 + 1, "different sets: bumped");
        // The in-band SPS refreshed the traits: profile, constraint byte, level.
        let s = tap.stream().unwrap();
        assert_eq!(
            (s.traits.profile, s.traits.constraint_flags, s.traits.level),
            (66, 0xC0, 0x0D),
            "constraint_flags is the SPS byte after profile_idc"
        );
        assert_eq!((s.traits.chroma, s.traits.bit_depth), (1, 8));
        // A reconnect is a parameter change and a new connection.
        let c = w.connection();
        let mut w2 = TapWriter::connect(Arc::clone(&tap), facts(), clock());
        assert_eq!(w2.connection(), c + 1);
        w2.on_packet(&idr_au(SPS2), Some(0), Some(0), true);
        assert!(reader.try_recv().unwrap().params_gen > g0 + 1);
        assert_eq!(tap.stream().unwrap().connection, c + 1);
    }

    #[test]
    fn a_rejected_scan_bumps_and_oversize_or_unstamped_is_skipped() {
        let tap = Arc::new(SourceTap::new());
        let reader = tap.tap().subscribe().unwrap();
        let mut w = TapWriter::connect(Arc::clone(&tap), facts(), clock());
        let g0 = tap.params_gen();
        // PPS before SPS: out of canonical order, the scan fails closed.
        let bad = annexb(&[PPS, SPS, &[0x65, 0x88]]);
        w.on_packet(&bad, Some(0), Some(0), true);
        assert_eq!(reader.try_recv().unwrap().params_gen, g0 + 1);
        w.on_packet(&vec![0_u8; MAX_AU_BYTES + 1], Some(1), Some(1), false);
        w.on_packet(&p_au(true), None, None, false);
        assert_eq!((tap.oversize(), tap.unstamped()), (1, 1));
        assert!(reader.try_recv().is_none());
        let mut no_run = TapWriter::connect(Arc::clone(&tap), facts(), Box::new(|| None));
        no_run.on_packet(&p_au(true), Some(2), Some(2), false);
        assert_eq!(tap.unstamped(), 2, "no run clock: skipped");
    }

    #[test]
    fn traits_read_the_avcc_sps_and_the_colour_tags() {
        let mut f = facts();
        // avcC: version 1, profile, compat, level, 0xFF, 1 SPS, len16, SPS.
        f.extradata = vec![
            1,
            0x42,
            0xC0,
            0x0D,
            0xFF,
            0xE1,
            0,
            u8::try_from(SPS2.len()).unwrap(),
        ];
        f.extradata.extend_from_slice(SPS2);
        f.color_primaries = 2;
        f.color_range = 2;
        f.field_order = 3;
        assert_eq!(extradata_sps(&f.extradata), Some(SPS2));
        let t = source_traits(&f, extradata_sps(&f.extradata));
        assert_eq!((t.profile, t.constraint_flags, t.level), (66, 0xC0, 0x0D));
        assert_eq!(t.scan, Scan::Progressive, "the SPS wins over libav");
        assert_eq!(t.colour.primaries, None, "unspecified is untagged");
        assert_eq!(t.colour.range, Some(1));
        let t = source_traits(&f, None);
        assert_eq!(t.scan, Scan::Interlaced);
        assert_eq!(t.constraint_flags, 0);
    }
}
