//! The ingest side of passthrough (output-routing §12.5, issue #344 D2a): one
//! [`SourceTap`] per source, found through a wait-free registry, and the
//! per-connection [`TapWriter`] the demux reader thread feeds.
//!
//! ## Cost and blocking (invariants #1 / #10)
//!
//! * Without a consumer the writer does one atomic load per packet
//!   ([`multiview_output::passthrough::CompressedTap::has_consumers`]) and
//!   returns: no copy, no classification, no allocation.
//! * With a consumer it copies the packet bytes once into an `Arc<[u8]>`
//!   (re-framed to Annex B in that copy; the size and NAL count are checked
//!   first, so nothing over `MAX_AU_BYTES` is ever allocated),
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
    /// The NAL framing of the source stream. Tap AUs are always re-framed
    /// as Annex B ([`TapAu::data`]), whatever this says.
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
    /// Connections refused because their extradata exceeded the bound.
    refused_extradata: AtomicU64,
    /// Packets skipped because their length prefixes were malformed.
    malformed: AtomicU64,
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
            refused_extradata: AtomicU64::new(0),
            malformed: AtomicU64::new(0),
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

    /// Connections refused for an extradata over
    /// `multiview_ffmpeg::MAX_FACTS_EXTRADATA` (passthrough disabled for
    /// that connection).
    #[must_use]
    pub fn refused_extradata(&self) -> u64 {
        self.refused_extradata.load(Ordering::Relaxed)
    }

    /// Packets skipped for malformed length prefixes (never re-framed).
    #[must_use]
    pub fn malformed(&self) -> u64 {
        self.malformed.load(Ordering::Relaxed)
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
    /// Codec extradata (avcC or Annex-B sets; may be empty). Bounded by
    /// `multiview_ffmpeg::MAX_FACTS_EXTRADATA` at the copy.
    pub extradata: Vec<u8>,
    /// The stream's extradata was over the bound and not copied: the
    /// connection gets no writer ([`TapWriter::try_connect`]).
    pub extradata_oversize: bool,
}

/// Largest SPS NAL the tap extracts and keeps (4 KiB, as D1's
/// `h264_repeat` parameter-set bound).
pub const MAX_SPS_BYTES: usize = 4096;

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
        if len > MAX_SPS_BYTES {
            return None;
        }
        return extradata.get(8..8_usize.checked_add(len)?);
    }
    idr::h264_nal_of_type(extradata, NalFraming::AnnexB, 7).filter(|s| s.len() <= MAX_SPS_BYTES)
}

/// The first SPS and PPS of H.264 extradata (an avcC record or Annex-B
/// sets), each bounded by [`MAX_SPS_BYTES`]; `None` when either is missing,
/// truncated or over the bound.
fn extradata_sets(extradata: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if extradata.first() == Some(&1) {
        let mut at = 5_usize;
        let sps_count = extradata.get(at)? & 0x1F;
        at += 1;
        let mut sps: Option<&[u8]> = None;
        for _ in 0..sps_count {
            let len = usize::from(u16::from_be_bytes([
                *extradata.get(at)?,
                *extradata.get(at + 1)?,
            ]));
            let set = extradata.get(at + 2..at.checked_add(2)?.checked_add(len)?)?;
            sps = sps.or(Some(set));
            at += 2 + len;
        }
        let pps_count = *extradata.get(at)?;
        at += 1;
        let mut pps: Option<&[u8]> = None;
        for _ in 0..pps_count {
            let len = usize::from(u16::from_be_bytes([
                *extradata.get(at)?,
                *extradata.get(at + 1)?,
            ]));
            let set = extradata.get(at + 2..at.checked_add(2)?.checked_add(len)?)?;
            pps = pps.or(Some(set));
            at += 2 + len;
        }
        let (sps, pps) = (sps?, pps?);
        return (sps.len() <= MAX_SPS_BYTES
            && pps.len() <= MAX_SPS_BYTES
            && !sps.is_empty()
            && !pps.is_empty())
        .then(|| (sps.to_vec(), pps.to_vec()));
    }
    let sps = idr::h264_nal_of_type(extradata, NalFraming::AnnexB, 7)?;
    let pps = idr::h264_nal_of_type(extradata, NalFraming::AnnexB, 8)?;
    (sps.len() <= MAX_SPS_BYTES && pps.len() <= MAX_SPS_BYTES).then(|| (sps.to_vec(), pps.to_vec()))
}

/// An Annex-B IDR `au` that lacks its parameter sets, with `sps` and `pps`
/// inserted after a leading access unit delimiter (or at the front), in one
/// exact-size allocation. `None` when the result would exceed `max_bytes`.
fn with_sets(au: &[u8], sps: &[u8], pps: &[u8], max_bytes: usize) -> Option<Arc<[u8]>> {
    // The two inserted NALs count against the AU's NAL budget too.
    idr::annex_b_nal_count(au, idr::MAX_AU_NALS.saturating_sub(2))?;
    // An AUD, when it is the first NAL, stays first (H.264 7.4.1.2.3):
    // insert after it, at the next start code.
    let first_nal_at = au.windows(3).position(|w| w == [0, 0, 1]).map(|p| p + 3)?;
    let split = if au.get(first_nal_at).is_some_and(|&b| b & 0x1F == 9) {
        au.get(first_nal_at..)?
            .windows(3)
            .position(|w| w == [0, 0, 1])
            .map_or(au.len(), |p| {
                // Back up over a 4-byte start code's leading zero.
                let sc = first_nal_at + p;
                if au.get(sc - 1) == Some(&0) {
                    sc - 1
                } else {
                    sc
                }
            })
    } else {
        0
    };
    let total = au
        .len()
        .checked_add(8)?
        .checked_add(sps.len())?
        .checked_add(pps.len())?;
    if total > max_bytes {
        return None;
    }
    let mut out: Arc<[u8]> = std::iter::repeat_n(0_u8, total).collect();
    let dst = Arc::get_mut(&mut out)?;
    let mut at = 0_usize;
    for part in [
        au.get(..split)?,
        &[0, 0, 0, 1],
        sps,
        &[0, 0, 0, 1],
        pps,
        au.get(split..)?,
    ] {
        dst.get_mut(at..at.checked_add(part.len())?)?
            .copy_from_slice(part);
        at += part.len();
    }
    Some(out)
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
    /// The current H.264 SPS/PPS: from the extradata at connect, replaced by
    /// any in-band SPS/PPS as they pass. Prepended to an IDR that lacks
    /// them (an avcC source carries them only in extradata).
    current_sets: Option<(Vec<u8>, Vec<u8>)>,
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
    /// [`Self::connect`], unless the connection's extradata was over the
    /// bound (`facts.extradata_oversize`): then no writer exists for the
    /// connection, the previous stream description is withdrawn and the
    /// refusal is counted ([`SourceTap::refused_extradata`]).
    #[must_use]
    pub fn try_connect(
        tap: Arc<SourceTap>,
        facts: OpenFacts,
        now_ns: Box<dyn Fn() -> Option<u64> + Send>,
    ) -> Option<Self> {
        if facts.extradata_oversize {
            tap.refused_extradata.fetch_add(1, Ordering::Relaxed);
            tap.stream.store(None);
            return None;
        }
        Some(Self::connect(tap, facts, now_ns))
    }

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
        let current_sets = match facts.codec {
            CodecKind::H264 => extradata_sets(&facts.extradata),
            _ => None,
        };
        let mut writer = Self {
            current_sets,
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

    /// In-band sets replace the cached ones; an H.264 IDR that carries
    /// neither SPS nor PPS gets the current ones prepended (never a
    /// duplicate). `None` when the completed AU would exceed `MAX_AU_BYTES`.
    fn complete_idr(&mut self, bytes: Arc<[u8]>) -> Option<Arc<[u8]>> {
        let framing = NalFraming::AnnexB;
        if self.facts.codec != CodecKind::H264 || !idr::is_idr(&bytes, CodecKind::H264, framing) {
            return Some(bytes);
        }
        let bounded =
            |t| idr::h264_nal_of_type(&bytes, framing, t).filter(|s| s.len() <= MAX_SPS_BYTES);
        match (bounded(7), bounded(8), self.current_sets.as_mut()) {
            (Some(sps), Some(pps), _) => {
                self.current_sets = Some((sps.to_vec(), pps.to_vec()));
                Some(bytes)
            }
            (None, None, Some((sps, pps))) => with_sets(&bytes, sps, pps, MAX_AU_BYTES),
            (Some(sps), None, Some((cached, _))) => {
                *cached = sps.to_vec();
                Some(bytes)
            }
            (None, Some(pps), Some((_, cached))) => {
                *cached = pps.to_vec();
                Some(bytes)
            }
            _ => Some(bytes),
        }
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
        // Every tap AU is Annex B (the one copy re-frames a length-prefixed
        // packet), so a reader never depends on a connection's framing and
        // the bytes go straight into an MPEG-TS muxer.
        // The re-framed size and NAL count are checked before allocating:
        // an expansion past MAX_AU_BYTES (1/2-byte prefixes) is counted as
        // oversize here, never built and then dropped by the offer.
        let bytes: Arc<[u8]> = match idr::annex_b_size(data, self.facts.framing, MAX_AU_BYTES)
            .map(|_| idr::to_annex_b(data, self.facts.framing, MAX_AU_BYTES))
        {
            Ok(Some(bytes)) => bytes,
            Err(idr::ReframeError::TooLarge) => {
                self.tap.oversize.fetch_add(1, Ordering::Relaxed);
                return;
            }
            Ok(None) | Err(idr::ReframeError::Malformed) => {
                self.tap.malformed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let framing = NalFraming::AnnexB;
        let Some(bytes) = self.complete_idr(bytes) else {
            self.tap.oversize.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let data = &bytes[..];
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
                        let sps = sps.filter(|s| s.len() <= MAX_SPS_BYTES);
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
            data: bytes,
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
            extradata_oversize: false,
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

    #[test]
    fn length_prefixed_packets_reach_the_reader_as_annex_b() {
        let tap = Arc::new(SourceTap::new());
        let reader = tap.tap().subscribe().unwrap();
        let mut f = facts();
        f.framing = NalFraming::LengthPrefixed { nal_length_size: 4 };
        let mut w = TapWriter::connect(Arc::clone(&tap), f, clock());
        let mut lp = Vec::new();
        for nal in [SPS, PPS, &[0x65, 0x88, 0x80][..]] {
            lp.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
            lp.extend_from_slice(nal);
        }
        w.on_packet(&lp, Some(0), Some(0), true);
        let au = reader.try_recv().unwrap();
        assert_eq!(&au.data[..], &idr_au(SPS)[..]);
        assert!(au.strict_idr);
    }

    /// #344 D2a review R1-A-1: a connection whose extradata was over the
    /// bound gets no writer (passthrough disabled for it, never a truncated
    /// configuration), counted under its own reason; the tap stays usable.
    #[test]
    fn an_oversize_extradata_connection_gets_no_writer_and_is_counted() {
        let tap = Arc::new(SourceTap::new());
        let mut f = facts();
        f.extradata_oversize = true;
        assert!(TapWriter::try_connect(Arc::clone(&tap), f, clock()).is_none());
        assert_eq!(tap.refused_extradata(), 1);
        assert!(tap.stream().is_none(), "no traits published");
        assert!(TapWriter::try_connect(Arc::clone(&tap), facts(), clock()).is_some());
        assert_eq!(tap.refused_extradata(), 1);
    }

    /// SPS extraction is bounded: an avcC SPS length past the record (or
    /// over the SPS bound) yields no SPS, never a read past the record.
    #[test]
    fn avcc_sps_extraction_is_bounded() {
        let mut rec = vec![1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0xFF, 0xFF];
        rec.extend_from_slice(&[0x67; 16]);
        assert_eq!(extradata_sps(&rec), None);
        let mut huge = vec![1, 0x64, 0, 0x1F, 0xFF, 0xE1, 0x20, 0x00];
        huge.extend(std::iter::repeat_n(0x67, 0x2000));
        assert_eq!(extradata_sps(&huge), None, "over MAX_SPS_BYTES");
    }

    /// #344 D2a review R1-A-2: a length-prefixed packet within `MAX_AU_BYTES`
    /// whose Annex-B form would exceed it is refused before re-framing and
    /// counted as oversize at once; so is one over the NAL budget.
    #[test]
    fn length_prefixed_expansion_past_the_cap_is_counted_oversize_before_allocating() {
        let tap = Arc::new(SourceTap::new());
        let reader = tap.tap().subscribe().unwrap();
        let mut f = facts();
        f.framing = NalFraming::LengthPrefixed { nal_length_size: 2 };
        let mut w = TapWriter::connect(Arc::clone(&tap), f, clock());
        // 2-byte prefixes of 0xFFFF-byte NALs: input under the cap, Annex-B
        // form (+2 bytes per NAL) over it.
        // 127 NALs of 0xFFFF bytes plus one filler NAL: exactly MAX_AU_BYTES
        // in, MAX_AU_BYTES + 256 out (each NAL grows by 2 bytes).
        let nal = 0xFFFF_usize;
        let mut lp = Vec::with_capacity(MAX_AU_BYTES);
        for _ in 0..127 {
            lp.extend_from_slice(&[0xFF, 0xFF, 0x41]);
            lp.resize(lp.len() + nal - 1, 0x11);
        }
        let filler = MAX_AU_BYTES - lp.len() - 2;
        lp.extend_from_slice(&u16::try_from(filler).unwrap().to_be_bytes());
        lp.push(0x41);
        lp.resize(MAX_AU_BYTES, 0x11);
        assert_eq!(lp.len(), MAX_AU_BYTES);
        assert_eq!(
            idr::annex_b_size(
                &lp,
                NalFraming::LengthPrefixed { nal_length_size: 2 },
                usize::MAX
            ),
            Ok(MAX_AU_BYTES + 2 * 128)
        );
        w.on_packet(&lp, Some(0), Some(0), false);
        assert_eq!(tap.oversize(), 1);
        assert!(reader.try_recv().is_none());
        // 1-byte prefixes, 2000 one-byte NALs: over the NAL budget.
        let mut g = facts();
        g.framing = NalFraming::LengthPrefixed { nal_length_size: 1 };
        let mut w = TapWriter::connect(Arc::clone(&tap), g, clock());
        let many: Vec<u8> = (0..2000).flat_map(|_| [1_u8, 0x41]).collect();
        w.on_packet(&many, Some(1), Some(1), false);
        assert_eq!(tap.oversize(), 2);
        assert!(reader.try_recv().is_none());
    }

    /// A libx264 mp4 (avcC: SPS/PPS only in extradata, `repeat-headers=0`).
    fn avcc_clip(path: &std::path::Path) {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc2=size=160x120:rate=25:duration=2")
            .args(["-pix_fmt", "yuv420p", "-c:v", "libx264", "-x264-params"])
            .arg("keyint=10:min-keyint=10:scenecut=0:bframes=0:repeat-headers=0")
            .args(["-f", "mp4"])
            .arg(path)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success());
    }

    fn open_facts(
        codec: CodecKind,
        framing: NalFraming,
        time_base: Rational,
        raw: multiview_ffmpeg::VideoStreamFacts,
    ) -> OpenFacts {
        OpenFacts {
            codec,
            framing,
            time_base,
            frame_rate: Rational::new(25, 1),
            profile: raw.profile,
            level: raw.level,
            width: raw.width,
            height: raw.height,
            color_primaries: raw.color_primaries,
            color_trc: raw.color_trc,
            color_space: raw.color_space,
            color_range: raw.color_range,
            field_order: raw.field_order,
            bits_per_raw_sample: raw.bits_per_raw_sample,
            extradata: raw.extradata,
            extradata_oversize: raw.extradata_oversize,
        }
    }

    /// #344 D2a review R1-M-1: an avcC source (MP4/avc1) carries its SPS/PPS
    /// only in extradata, so its IDR packets are not self-contained. The
    /// tap prepends the cached sets to such IDRs: every IDR AU is strict, a
    /// passthrough clock starts a run on it, and the emitted AUs decode.
    #[test]
    fn an_avcc_source_starts_a_run_and_its_aus_decode() {
        use multiview_output::passthrough::{Emission, PassthroughClock, RepeatSource, SlotGate};
        struct Open;
        impl SlotGate for Open {
            fn matches(&self, _: u64) -> bool {
                true
            }
            fn matched_slate_ready(&self, _: u64) -> bool {
                true
            }
        }
        struct NoRepeat;
        impl RepeatSource for NoRepeat {
            fn repeat_after(&mut self, _: &TapAu) -> Option<Arc<[u8]>> {
                None
            }
        }
        multiview_ffmpeg::ensure_initialized().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("avcc.mp4");
        avcc_clip(&path);
        let mut input = ffmpeg_next::format::input(&path).unwrap();
        let stream = input
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .unwrap();
        let index = stream.index();
        let params = stream.parameters();
        let (codec, framing) = multiview_ffmpeg::stream_framing(&params).unwrap();
        assert!(matches!(framing, NalFraming::LengthPrefixed { .. }), "avcC");
        let f = open_facts(
            codec,
            framing,
            multiview_ffmpeg::from_ff_rational(stream.time_base()),
            multiview_ffmpeg::video_stream_facts(&params),
        );
        drop(params);
        let tap = Arc::new(SourceTap::new());
        let reader = tap.tap().subscribe().unwrap();
        let mut w = TapWriter::connect(Arc::clone(&tap), f, clock());
        let mut raw_idrs_without_sets = 0;
        for (s, packet) in input.packets() {
            if s.index() != index {
                continue;
            }
            let bytes = packet.data().unwrap();
            if packet.is_key() && !idr::is_self_contained_idr(bytes, CodecKind::H264, framing) {
                raw_idrs_without_sets += 1;
            }
            w.on_packet(bytes, packet.dts(), packet.pts(), packet.is_key());
        }
        assert!(
            raw_idrs_without_sets >= 4,
            "the source IDRs lack in-band sets"
        );
        let aus: Vec<TapAu> = reader.drain().collect();
        assert_eq!(aus.len(), 50);
        let idrs: Vec<&TapAu> = aus
            .iter()
            .filter(|a| idr::is_idr(&a.data, CodecKind::H264, NalFraming::AnnexB))
            .collect();
        assert!(idrs.len() >= 4);
        assert!(idrs.iter().all(|a| a.strict_idr), "every IDR is strict");
        assert!(aus.iter().all(|a| a.params_gen == aus[0].params_gen));
        // A clock starts a run on the first IDR and passes every AU through.
        let mut c = PassthroughClock::new(Rational::new(25, 1));
        c.arm();
        let mut emitted = Vec::new();
        let mut it = aus.into_iter();
        for tick in 0..60_u64 {
            let next: Vec<TapAu> = it.next().into_iter().collect();
            if let Emission::Au { data, .. } =
                c.on_tick(tick, 7, next.into_iter(), &Open, &mut NoRepeat)
            {
                emitted.push(data);
            }
        }
        assert!(emitted.len() >= 45, "a run started: {} AUs", emitted.len());
        let mut dec = multiview_ffmpeg::H264PacketDecoder::new(Rational::new(1, 25)).unwrap();
        let mut frames = 0;
        for (i, au) in emitted.iter().enumerate() {
            dec.push(au, Some(i64::try_from(i).unwrap())).unwrap();
            while dec.receive_frame().unwrap().is_some() {
                frames += 1;
            }
        }
        dec.send_eof().unwrap();
        while dec.receive_frame().unwrap().is_some() {
            frames += 1;
        }
        assert_eq!(frames, emitted.len(), "every emitted AU decodes");
    }

    /// The cached sets go after a leading AUD and before the IDR slice.
    #[test]
    fn sets_are_inserted_after_a_leading_aud() {
        let au = annexb(&[&[0x09, 0x10], &[0x65, 0x88, 0x80]]);
        let full = with_sets(&au, SPS, PPS, MAX_AU_BYTES).unwrap();
        assert_eq!(
            &full[..],
            &annexb(&[&[0x09, 0x10], SPS, PPS, &[0x65, 0x88, 0x80]])[..]
        );
        let bare = annexb(&[&[0x65, 0x88, 0x80]]);
        let full = with_sets(&bare, SPS, PPS, MAX_AU_BYTES).unwrap();
        assert_eq!(&full[..], &annexb(&[SPS, PPS, &[0x65, 0x88, 0x80]])[..]);
        assert!(
            with_sets(&bare, SPS, PPS, bare.len() + 8).is_none(),
            "bounded"
        );
    }

    /// #344 D2a review R2-A-3: an Annex-B packet with more than `MAX_AU_NALS`
    /// NALs is refused before any copy or classification, counted, and
    /// never offered; an IDR at exactly the budget does not get sets
    /// inserted past it.
    #[test]
    fn an_annex_b_packet_over_the_nal_budget_is_refused_and_counted() {
        let tap = Arc::new(SourceTap::new());
        let reader = tap.tap().subscribe().unwrap();
        let mut w = TapWriter::connect(Arc::clone(&tap), facts(), clock());
        let many: Vec<u8> = (0..=idr::MAX_AU_NALS)
            .flat_map(|_| [0_u8, 0, 1, 0x41])
            .collect();
        w.on_packet(&many, Some(0), Some(0), false);
        assert_eq!(tap.oversize(), 1);
        assert!(reader.try_recv().is_none());
        // A set-less IDR at exactly the budget: completing it would exceed it.
        let mut w = TapWriter::connect(Arc::clone(&tap), facts(), clock());
        w.on_packet(&idr_au(SPS), Some(0), Some(0), true);
        assert!(reader.try_recv().is_some());
        let full: Vec<u8> = (0..idr::MAX_AU_NALS - 1)
            .flat_map(|_| [0_u8, 0, 1, 0x06, 0x05])
            .chain([0, 0, 1, 0x65, 0x88, 0x80])
            .collect();
        w.on_packet(&full, Some(3600), Some(3600), true);
        assert_eq!(tap.oversize(), 2);
        assert!(reader.try_recv().is_none());
    }
}
