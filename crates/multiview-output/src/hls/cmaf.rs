//! Pure-Rust CMAF (fragmented MP4) box writer for the Cast packager.
//!
//! Builds the byte vectors of single-track CMAF files: an init segment
//! (`ftyp` + `moov`) for one H.264 video track or one AAC audio track, and
//! media segments (`styp` + `moof` + `mdat`). It also carries the small
//! bitstream helpers the packager needs: H.264 access-unit re-framing
//! (Annex B or AVCC in, AVCC out), parameter-set extraction, the RFC 6381
//! `avc1.PPCCLL` codec string, ADTS header stripping and the AAC
//! `AudioSpecificConfig`.
//!
//! No I/O, no `unsafe`, no native dependency: this is compiled in the default
//! build. Every size is computed from the bytes actually written, so boxes
//! always nest exactly.
use crate::error::{Error, Result};

/// The single track id every cast rendition uses (one track per file set).
pub const TRACK_ID: u32 = 1;

/// `trun` sample flags of a sync sample (`sample_depends_on = 2`: depends on
/// no other sample).
pub const SAMPLE_FLAGS_SYNC: u32 = 0x0200_0000;
/// `trun` sample flags of a non-sync sample (`sample_depends_on = 1` and
/// `sample_is_non_sync_sample`).
pub const SAMPLE_FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// Saturating `usize` to `u32` (box sizes and counts are bounded by the
/// callers; saturation only guards the conversion).
fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_zeros(out: &mut Vec<u8>, count: usize) {
    out.resize(out.len().saturating_add(count), 0);
}

/// Append one box: `size` + `fourcc` + whatever `body` writes.
fn put_box(out: &mut Vec<u8>, fourcc: [u8; 4], body: impl FnOnce(&mut Vec<u8>)) {
    let mut content = Vec::new();
    body(&mut content);
    put_u32(out, to_u32(content.len().saturating_add(8)));
    out.extend_from_slice(&fourcc);
    out.extend_from_slice(&content);
}

/// Append one full box (`version` + 24-bit `flags` ahead of the body).
fn put_full_box(
    out: &mut Vec<u8>,
    fourcc: [u8; 4],
    version: u8,
    flags: u32,
    body: impl FnOnce(&mut Vec<u8>),
) {
    put_box(out, fourcc, |b| {
        put_u32(b, (u32::from(version) << 24) | (flags & 0x00FF_FFFF));
        body(b);
    });
}

/// The identity transformation matrix of `mvhd` / `tkhd`.
fn put_matrix(out: &mut Vec<u8>) {
    for value in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        put_u32(out, value);
    }
}

fn put_ftyp(out: &mut Vec<u8>) {
    put_box(out, *b"ftyp", |b| {
        b.extend_from_slice(b"iso6");
        put_u32(b, 0);
        b.extend_from_slice(b"iso6");
        b.extend_from_slice(b"cmfc");
        b.extend_from_slice(b"mp41");
    });
}

fn put_mvhd(out: &mut Vec<u8>, timescale: u32) {
    put_full_box(out, *b"mvhd", 0, 0, |b| {
        put_u32(b, 0); // creation_time
        put_u32(b, 0); // modification_time
        put_u32(b, timescale);
        put_u32(b, 0); // duration (fragmented: unknown)
        put_u32(b, 0x0001_0000); // rate 1.0
        put_u16(b, 0x0100); // volume 1.0
        put_zeros(b, 10);
        put_matrix(b);
        put_zeros(b, 24); // pre_defined
        put_u32(b, TRACK_ID.saturating_add(1)); // next_track_ID
    });
}

fn put_tkhd(out: &mut Vec<u8>, width: u32, height: u32, audio: bool) {
    // flags: track_enabled | track_in_movie | track_in_preview.
    put_full_box(out, *b"tkhd", 0, 7, |b| {
        put_u32(b, 0); // creation_time
        put_u32(b, 0); // modification_time
        put_u32(b, TRACK_ID);
        put_u32(b, 0); // reserved
        put_u32(b, 0); // duration
        put_zeros(b, 8);
        put_u16(b, 0); // layer
        put_u16(b, 0); // alternate_group
        put_u16(b, if audio { 0x0100 } else { 0 }); // volume
        put_u16(b, 0);
        put_matrix(b);
        put_u32(b, width.min(0xFFFF) << 16);
        put_u32(b, height.min(0xFFFF) << 16);
    });
}

fn put_mdhd(out: &mut Vec<u8>, timescale: u32) {
    put_full_box(out, *b"mdhd", 0, 0, |b| {
        put_u32(b, 0);
        put_u32(b, 0);
        put_u32(b, timescale);
        put_u32(b, 0);
        put_u16(b, 0x55C4); // language "und"
        put_u16(b, 0);
    });
}

fn put_hdlr(out: &mut Vec<u8>, handler: [u8; 4], name: &[u8]) {
    put_full_box(out, *b"hdlr", 0, 0, |b| {
        put_u32(b, 0); // pre_defined
        b.extend_from_slice(&handler);
        put_zeros(b, 12);
        b.extend_from_slice(name);
        b.push(0);
    });
}

fn put_dinf(out: &mut Vec<u8>) {
    put_box(out, *b"dinf", |b| {
        put_full_box(b, *b"dref", 0, 0, |d| {
            put_u32(d, 1);
            // flags = 1: the media data is in the same file (self-contained).
            put_full_box(d, *b"url ", 0, 1, |_| {});
        });
    });
}

/// `stbl` with the given sample entry and the four empty sample tables a
/// fragmented track carries.
fn put_stbl(out: &mut Vec<u8>, sample_entry: impl FnOnce(&mut Vec<u8>)) {
    put_box(out, *b"stbl", |b| {
        put_full_box(b, *b"stsd", 0, 0, |s| {
            put_u32(s, 1);
            sample_entry(s);
        });
        put_full_box(b, *b"stts", 0, 0, |s| put_u32(s, 0));
        put_full_box(b, *b"stsc", 0, 0, |s| put_u32(s, 0));
        put_full_box(b, *b"stsz", 0, 0, |s| {
            put_u32(s, 0);
            put_u32(s, 0);
        });
        put_full_box(b, *b"stco", 0, 0, |s| put_u32(s, 0));
    });
}

fn put_mvex(out: &mut Vec<u8>) {
    put_box(out, *b"mvex", |b| {
        put_full_box(b, *b"trex", 0, 0, |t| {
            put_u32(t, TRACK_ID);
            put_u32(t, 1); // default_sample_description_index
            put_u32(t, 0); // default_sample_duration
            put_u32(t, 0); // default_sample_size
            put_u32(t, 0); // default_sample_flags
        });
    });
}

/// Profiles whose `avcC` carries the chroma-format / bit-depth trailer and
/// whose SPS carries `chroma_format_idc`.
fn is_high_profile(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// MSB-first bit reader over an RBSP (emulation-prevention bytes removed).
struct BitReader {
    data: Vec<u8>,
    bit: usize,
}

impl BitReader {
    /// Strip `00 00 03` emulation-prevention bytes from a NAL payload.
    fn from_nal_payload(payload: &[u8]) -> Self {
        let mut data = Vec::with_capacity(payload.len());
        let mut zeros = 0u8;
        for &byte in payload {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 {
                zeros.saturating_add(1)
            } else {
                0
            };
            data.push(byte);
        }
        Self { data, bit: 0 }
    }

    fn read_bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.bit / 8)?;
        let shift = 7usize.checked_sub(self.bit % 8)?;
        self.bit = self.bit.saturating_add(1);
        Some(u32::from((byte >> shift) & 1))
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    fn read_ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.read_bit()? == 0 {
            leading = leading.saturating_add(1);
            if leading > 31 {
                return None;
            }
        }
        let mut value = 0u32;
        for _ in 0..leading {
            value = (value << 1) | self.read_bit()?;
        }
        Some(value.saturating_add((1u32 << leading).saturating_sub(1)))
    }
}

/// `(chroma_format_idc, bit_depth_luma_minus8, bit_depth_chroma_minus8)` of a
/// high-profile SPS NAL (header byte included), or `None` when it cannot be
/// parsed.
fn sps_chroma_and_depth(sps: &[u8]) -> Option<(u8, u8, u8)> {
    let mut bits = BitReader::from_nal_payload(sps.get(4..)?);
    let _seq_parameter_set_id = bits.read_ue()?;
    let chroma = bits.read_ue()?;
    if chroma == 3 {
        let _separate_colour_plane_flag = bits.read_bit()?;
    }
    let luma = bits.read_ue()?;
    let chroma_depth = bits.read_ue()?;
    if chroma > 3 || luma > 6 || chroma_depth > 6 {
        return None;
    }
    Some((
        u8::try_from(chroma).ok()?,
        u8::try_from(luma).ok()?,
        u8::try_from(chroma_depth).ok()?,
    ))
}

/// Build the `AVCDecoderConfigurationRecord` (the `avcC` payload) from one SPS
/// and one PPS NAL unit (each with its NAL header byte, no start code).
#[must_use]
pub fn avc_decoder_configuration_record(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let profile = sps.get(1).copied().unwrap_or(0);
    let compat = sps.get(2).copied().unwrap_or(0);
    let level = sps.get(3).copied().unwrap_or(0);
    let mut out = vec![1, profile, compat, level];
    out.push(0xFC | 3); // lengthSizeMinusOne = 3 (4-byte NAL lengths)
    out.push(0xE0 | 1); // one SPS
    put_u16(&mut out, u16::try_from(sps.len()).unwrap_or(u16::MAX));
    out.extend_from_slice(sps);
    out.push(1); // one PPS
    put_u16(&mut out, u16::try_from(pps.len()).unwrap_or(u16::MAX));
    out.extend_from_slice(pps);
    if is_high_profile(profile) {
        // 4:2:0 8-bit is the fallback when the SPS cannot be parsed.
        let (chroma, luma, chroma_depth) = sps_chroma_and_depth(sps).unwrap_or((1, 0, 0));
        out.push(0xFC | chroma);
        out.push(0xF8 | luma);
        out.push(0xF8 | chroma_depth);
        out.push(0); // numOfSequenceParameterSetExt
    }
    out
}

fn put_avc1(out: &mut Vec<u8>, width: u32, height: u32, sps: &[u8], pps: &[u8]) {
    put_box(out, *b"avc1", |b| {
        put_zeros(b, 6);
        put_u16(b, 1); // data_reference_index
        put_zeros(b, 16); // pre_defined / reserved
        put_u16(b, u16::try_from(width).unwrap_or(u16::MAX));
        put_u16(b, u16::try_from(height).unwrap_or(u16::MAX));
        put_u32(b, 0x0048_0000); // 72 dpi
        put_u32(b, 0x0048_0000);
        put_u32(b, 0);
        put_u16(b, 1); // frame_count
        put_zeros(b, 32); // compressorname
        put_u16(b, 0x0018); // depth
        put_u16(b, 0xFFFF); // pre_defined = -1
        put_box(b, *b"avcC", |c| {
            c.extend_from_slice(&avc_decoder_configuration_record(sps, pps));
        });
    });
}

/// Build the init segment (`ftyp` + `moov`) of a single H.264 video track.
///
/// `timescale` is the media timescale of the track, `sps` / `pps` are single
/// NAL units (header byte included, no start code or length prefix).
#[must_use]
pub fn video_init_segment(
    timescale: u32,
    width: u32,
    height: u32,
    sps: &[u8],
    pps: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    put_ftyp(&mut out);
    put_box(&mut out, *b"moov", |moov| {
        put_mvhd(moov, timescale);
        put_box(moov, *b"trak", |trak| {
            put_tkhd(trak, width, height, false);
            put_box(trak, *b"mdia", |mdia| {
                put_mdhd(mdia, timescale);
                put_hdlr(mdia, *b"vide", b"VideoHandler");
                put_box(mdia, *b"minf", |minf| {
                    put_full_box(minf, *b"vmhd", 0, 1, |v| put_zeros(v, 8));
                    put_dinf(minf);
                    put_stbl(minf, |s| put_avc1(s, width, height, sps, pps));
                });
            });
        });
        put_mvex(moov);
    });
    out
}

/// Append an MPEG-4 descriptor: tag, expandable length, payload.
fn put_descriptor(out: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    out.push(tag);
    let len = to_u32(payload.len()).min(0x0FFF_FFFF);
    let mut started = false;
    for shift in [21u32, 14, 7] {
        let group = u8::try_from((len >> shift) & 0x7F).unwrap_or(0);
        if group != 0 || started {
            out.push(group | 0x80);
            started = true;
        }
    }
    out.push(u8::try_from(len & 0x7F).unwrap_or(0));
    out.extend_from_slice(payload);
}

/// The `esds` payload (after the full-box header): an `ES_Descriptor` holding
/// the AAC `DecoderConfigDescriptor` (object type `0x40`, audio stream) with
/// the `AudioSpecificConfig` as its `DecoderSpecificInfo`.
fn es_descriptor(asc: &[u8], max_bitrate: u32, avg_bitrate: u32) -> Vec<u8> {
    let mut decoder_config = vec![0x40, 0x15, 0, 0, 0];
    put_u32(&mut decoder_config, max_bitrate);
    put_u32(&mut decoder_config, avg_bitrate);
    put_descriptor(&mut decoder_config, 0x05, asc);
    let mut es = vec![0, 0, 0]; // ES_ID = 0, no flags
    put_descriptor(&mut es, 0x04, &decoder_config);
    put_descriptor(&mut es, 0x06, &[0x02]); // SLConfig: MP4 predefined
    let mut out = Vec::new();
    put_descriptor(&mut out, 0x03, &es);
    out
}

/// Build the init segment (`ftyp` + `moov`) of a single AAC audio track.
///
/// The media timescale is the sample rate. `audio_specific_config` is the
/// codec's `AudioSpecificConfig`; `max_bitrate` / `avg_bitrate` go into the
/// `esds` (bits per second, `0` when unknown).
#[must_use]
pub fn audio_init_segment(
    sample_rate: u32,
    channels: u16,
    audio_specific_config: &[u8],
    max_bitrate: u32,
    avg_bitrate: u32,
) -> Vec<u8> {
    let mut out = Vec::new();
    put_ftyp(&mut out);
    put_box(&mut out, *b"moov", |moov| {
        put_mvhd(moov, sample_rate);
        put_box(moov, *b"trak", |trak| {
            put_tkhd(trak, 0, 0, true);
            put_box(trak, *b"mdia", |mdia| {
                put_mdhd(mdia, sample_rate);
                put_hdlr(mdia, *b"soun", b"SoundHandler");
                put_box(mdia, *b"minf", |minf| {
                    put_full_box(minf, *b"smhd", 0, 0, |s| put_zeros(s, 4));
                    put_dinf(minf);
                    put_stbl(minf, |s| {
                        put_box(s, *b"mp4a", |b| {
                            put_zeros(b, 6);
                            put_u16(b, 1); // data_reference_index
                            put_zeros(b, 8);
                            put_u16(b, channels);
                            put_u16(b, 16); // samplesize
                            put_u32(b, 0);
                            // 16.16 fixed point; rates above 65535 Hz carry 0
                            // here (the esds config is authoritative).
                            put_u32(b, u16::try_from(sample_rate).map_or(0, u32::from) << 16);
                            put_full_box(b, *b"esds", 0, 0, |e| {
                                e.extend_from_slice(&es_descriptor(
                                    audio_specific_config,
                                    max_bitrate,
                                    avg_bitrate,
                                ));
                            });
                        });
                    });
                });
            });
        });
        put_mvex(moov);
    });
    out
}

/// One sample of a media segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// Sample duration in the track timescale.
    pub duration: u32,
    /// `trun` sample flags ([`SAMPLE_FLAGS_SYNC`] / [`SAMPLE_FLAGS_NON_SYNC`]).
    pub flags: u32,
    /// Composition time offset (`pts - dts`) in the track timescale.
    pub composition_offset: i32,
    /// The sample bytes (AVCC-framed H.264 access unit, or one raw AAC frame).
    pub data: Vec<u8>,
}

/// Build one media segment: `styp` + `moof` + `mdat`.
///
/// `sequence_number` goes into `mfhd`, `base_media_decode_time` (track
/// timescale) into the version-1 `tfdt`. The `trun` carries a data offset
/// (the first `mdat` payload byte relative to the start of `moof`) and, per
/// sample, duration, size and flags; composition offsets are written (and the
/// `trun` becomes version 1, signed) only when some sample has a non-zero
/// offset.
///
/// # Errors
/// Returns [`Error::Output`] when the payload does not fit a 32-bit box.
pub fn media_segment(
    sequence_number: u32,
    base_media_decode_time: u64,
    samples: &[Sample],
) -> Result<Vec<u8>> {
    let payload: usize = samples
        .iter()
        .fold(0usize, |acc, s| acc.saturating_add(s.data.len()));
    let has_cts = samples.iter().any(|s| s.composition_offset != 0);
    let per_sample: usize = if has_cts { 16 } else { 12 };
    // moof = 8 + mfhd(16) + traf(8 + tfhd(16) + tfdt(20) + trun(20 + n * per)).
    let moof_size = samples.len().saturating_mul(per_sample).saturating_add(88);
    let total = moof_size.saturating_add(payload).saturating_add(8 + 24);
    if u32::try_from(total).is_err() || samples.iter().any(|s| u32::try_from(s.data.len()).is_err())
    {
        return Err(Error::Output(format!(
            "CMAF media segment of {total} bytes does not fit a 32-bit box"
        )));
    }
    let data_offset = to_u32(moof_size.saturating_add(8));
    let mut out = Vec::with_capacity(total);
    put_box(&mut out, *b"styp", |b| {
        b.extend_from_slice(b"msdh");
        put_u32(b, 0);
        b.extend_from_slice(b"msdh");
        b.extend_from_slice(b"msix");
    });
    put_box(&mut out, *b"moof", |moof| {
        put_full_box(moof, *b"mfhd", 0, 0, |b| put_u32(b, sequence_number));
        put_box(moof, *b"traf", |traf| {
            // default-base-is-moof: offsets are relative to this moof.
            put_full_box(traf, *b"tfhd", 0, 0x02_0000, |b| put_u32(b, TRACK_ID));
            put_full_box(traf, *b"tfdt", 1, 0, |b| put_u64(b, base_media_decode_time));
            let flags = 0x0001 | 0x0100 | 0x0200 | 0x0400 | if has_cts { 0x0800 } else { 0 };
            put_full_box(traf, *b"trun", u8::from(has_cts), flags, |b| {
                put_u32(b, to_u32(samples.len()));
                put_u32(b, data_offset);
                for sample in samples {
                    put_u32(b, sample.duration);
                    put_u32(b, to_u32(sample.data.len()));
                    put_u32(b, sample.flags);
                    if has_cts {
                        b.extend_from_slice(&sample.composition_offset.to_be_bytes());
                    }
                }
            });
        });
    });
    put_u32(&mut out, to_u32(payload.saturating_add(8)));
    out.extend_from_slice(b"mdat");
    for sample in samples {
        out.extend_from_slice(&sample.data);
    }
    Ok(out)
}

/// How the NAL units of an H.264 access unit are delimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalFraming {
    /// Annex B: `00 00 01` / `00 00 00 01` start codes.
    AnnexB,
    /// AVCC: a 4-byte big-endian length ahead of every NAL unit.
    Avcc,
}

/// Split an AVCC buffer into NAL units; `None` unless the 4-byte lengths walk
/// exactly to the end of the buffer over non-empty NAL units.
fn split_avcc(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut nals = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (len_bytes, tail) = rest.split_first_chunk::<4>()?;
        let len = usize::try_from(u32::from_be_bytes(*len_bytes)).ok()?;
        if len == 0 {
            return None;
        }
        let nal = tail.get(..len)?;
        // forbidden_zero_bit must be clear in every NAL header.
        if nal.first()? & 0x80 != 0 {
            return None;
        }
        nals.push(nal);
        rest = tail.get(len..)?;
    }
    if nals.is_empty() {
        None
    } else {
        Some(nals)
    }
}

/// Split an Annex B buffer at its start codes (3- or 4-byte). Zero bytes
/// ahead of a start code (the fourth start-code byte, `trailing_zero_8bits`)
/// are not part of the preceding NAL unit.
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    // Offsets of the first payload byte after every `00 00 01`.
    let mut starts = Vec::new();
    let mut i = 0usize;
    while let Some(window) = data.get(i..i.saturating_add(3)) {
        if window == [0, 0, 1] {
            starts.push(i.saturating_add(3));
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    let mut nals = Vec::new();
    for (index, &start) in starts.iter().enumerate() {
        let end = starts
            .get(index.saturating_add(1))
            .map_or(data.len(), |next| next.saturating_sub(3));
        let Some(mut nal) = data.get(start..end) else {
            continue;
        };
        while let Some((&0, head)) = nal.split_last() {
            nal = head;
        }
        if !nal.is_empty() {
            nals.push(nal);
        }
    }
    nals
}

/// Detect the framing of an access unit: Annex B when the buffer starts with
/// `00 00 00 01`, or with `00 00 01` and is not a well-formed AVCC buffer (an
/// AVCC unit whose first NAL is 256..=511 bytes long also starts `00 00 01`);
/// otherwise AVCC when the 4-byte lengths walk exactly to the end.
#[must_use]
pub fn detect_nal_framing(data: &[u8]) -> Option<NalFraming> {
    if data.starts_with(&[0, 0, 0, 1]) {
        return Some(NalFraming::AnnexB);
    }
    if split_avcc(data).is_some() {
        return Some(NalFraming::Avcc);
    }
    if data.starts_with(&[0, 0, 1]) {
        return Some(NalFraming::AnnexB);
    }
    None
}

/// Split an access unit into its NAL units (header byte first, no start code
/// or length prefix). `framing` forces the interpretation; `None`
/// auto-detects it with [`detect_nal_framing`].
///
/// # Errors
/// Returns [`Error::Output`] when the buffer is neither Annex B nor a
/// well-formed AVCC buffer, or holds no NAL unit.
pub fn split_nal_units(data: &[u8], framing: Option<NalFraming>) -> Result<Vec<&[u8]>> {
    let framing = framing.or_else(|| detect_nal_framing(data));
    let nals = match framing {
        Some(NalFraming::AnnexB) => split_annex_b(data),
        Some(NalFraming::Avcc) => split_avcc(data).unwrap_or_default(),
        None => Vec::new(),
    };
    if nals.is_empty() {
        return Err(Error::Output(
            "H.264 access unit is neither Annex B nor AVCC framed".to_owned(),
        ));
    }
    Ok(nals)
}

/// The `nal_unit_type` of a NAL unit (0 for an empty slice).
#[must_use]
pub fn nal_unit_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |header| header & 0x1F)
}

/// An H.264 access unit re-framed for a CMAF sample.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AccessUnit {
    /// The sample bytes: AVCC framing (4-byte lengths) with AUD, SPS, PPS and
    /// filler NAL units removed.
    pub data: Vec<u8>,
    /// Whether the unit holds an IDR slice (`nal_unit_type` 5).
    pub idr: bool,
    /// The first SPS NAL unit found in the unit, if any.
    pub sps: Option<Vec<u8>>,
    /// The first PPS NAL unit found in the unit, if any.
    pub pps: Option<Vec<u8>>,
    /// The framing the input used.
    pub framing: Option<NalFraming>,
}

/// Convert one access unit (Annex B or AVCC, see [`split_nal_units`]) into
/// AVCC sample bytes, dropping AUD (9), SPS (7), PPS (8) and filler (12) NAL
/// units and reporting the IDR flag and the parameter sets it carried.
///
/// # Errors
/// Returns [`Error::Output`] when the buffer cannot be split into NAL units.
pub fn h264_access_unit(data: &[u8], framing: Option<NalFraming>) -> Result<AccessUnit> {
    let framing = framing.or_else(|| detect_nal_framing(data));
    let nals = split_nal_units(data, framing)?;
    let mut unit = AccessUnit {
        data: Vec::with_capacity(data.len().saturating_add(8)),
        framing,
        ..AccessUnit::default()
    };
    for nal in nals {
        match nal_unit_type(nal) {
            7 => {
                if unit.sps.is_none() {
                    unit.sps = Some(nal.to_vec());
                }
            }
            8 => {
                if unit.pps.is_none() {
                    unit.pps = Some(nal.to_vec());
                }
            }
            9 | 12 => {}
            kind => {
                unit.idr |= kind == 5;
                put_u32(&mut unit.data, to_u32(nal.len()));
                unit.data.extend_from_slice(nal);
            }
        }
    }
    Ok(unit)
}

/// Read the first SPS and PPS out of encoder extradata in either form: an
/// `avcC` record (first byte `1`) or NAL units in Annex B / AVCC framing.
#[must_use]
pub fn avcc_or_annexb_parameter_sets(extradata: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    if extradata.first() == Some(&1) {
        if let Some(sets) = parameter_sets_from_avcc_record(extradata) {
            return Some(sets);
        }
    }
    let nals = split_nal_units(extradata, None).ok()?;
    let sps = nals.iter().find(|nal| nal_unit_type(nal) == 7)?;
    let pps = nals.iter().find(|nal| nal_unit_type(nal) == 8)?;
    Some((sps.to_vec(), pps.to_vec()))
}

/// Parse an `AVCDecoderConfigurationRecord` into its first SPS and PPS.
fn parameter_sets_from_avcc_record(record: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    fn read_sets(mut rest: &[u8], count: usize) -> Option<(Vec<Vec<u8>>, &[u8])> {
        let mut sets = Vec::new();
        for _ in 0..count {
            let (len_bytes, tail) = rest.split_first_chunk::<2>()?;
            let len = usize::from(u16::from_be_bytes(*len_bytes));
            sets.push(tail.get(..len)?.to_vec());
            rest = tail.get(len..)?;
        }
        Some((sets, rest))
    }
    let sps_count = usize::from(record.get(5)? & 0x1F);
    let (sps, rest) = read_sets(record.get(6..)?, sps_count)?;
    let (&pps_count, rest) = rest.split_first()?;
    let (pps, _) = read_sets(rest, usize::from(pps_count))?;
    let sps = sps.into_iter().next()?;
    let pps = pps.into_iter().next()?;
    (nal_unit_type(&sps) == 7 && nal_unit_type(&pps) == 8).then_some((sps, pps))
}

/// The RFC 6381 codec string `avc1.PPCCLL` of an SPS NAL unit (profile,
/// constraint flags and level: SPS bytes 1..=3 as upper-case hex).
#[must_use]
pub fn avc1_codec_string(sps: &[u8]) -> String {
    let byte = |index: usize| sps.get(index).copied().unwrap_or(0);
    format!("avc1.{:02X}{:02X}{:02X}", byte(1), byte(2), byte(3))
}

/// MPEG-4 sampling-frequency table (`samplingFrequencyIndex` 0..=12).
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The fields of an ADTS header that describe the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdtsHeader {
    /// Header length in bytes: 7, or 9 with a CRC.
    pub header_len: usize,
    /// `aac_frame_length`: header plus payload, in bytes.
    pub frame_len: usize,
    /// MPEG-4 audio object type (`profile + 1`; 2 = AAC-LC).
    pub object_type: u8,
    /// `samplingFrequencyIndex`.
    pub sampling_index: u8,
    /// `channel_configuration`.
    pub channel_config: u8,
}

impl AdtsHeader {
    /// The sample rate the header declares, when its index is a table entry.
    #[must_use]
    pub fn sample_rate(&self) -> Option<u32> {
        SAMPLE_RATES.get(usize::from(self.sampling_index)).copied()
    }

    /// The 2-byte `AudioSpecificConfig` this header describes.
    #[must_use]
    pub fn audio_specific_config(&self) -> Vec<u8> {
        asc_bytes(self.object_type, self.sampling_index, self.channel_config)
    }
}

/// `AudioSpecificConfig`: 5 bits object type, 4 bits sampling index, 4 bits
/// channel configuration, 3 zero bits (`GASpecificConfig`).
fn asc_bytes(object_type: u8, sampling_index: u8, channel_config: u8) -> Vec<u8> {
    let bits = (u16::from(object_type & 0x1F) << 11)
        | (u16::from(sampling_index & 0x0F) << 7)
        | (u16::from(channel_config & 0x0F) << 3);
    bits.to_be_bytes().to_vec()
}

/// Parse the ADTS header at the start of `frame`, when it starts with the
/// `0xFFF` syncword and is long enough.
#[must_use]
pub fn parse_adts_header(frame: &[u8]) -> Option<AdtsHeader> {
    let (head, _) = frame.split_first_chunk::<7>()?;
    let [b0, b1, b2, b3, b4, b5, _] = *head;
    // Syncword 0xFFF and layer == 0.
    if b0 != 0xFF || b1 & 0xF6 != 0xF0 {
        return None;
    }
    let header_len = if b1 & 1 == 1 { 7 } else { 9 };
    let frame_len = (usize::from(b3 & 0x03) << 11) | (usize::from(b4) << 3) | usize::from(b5 >> 5);
    if frame_len < header_len {
        return None;
    }
    Some(AdtsHeader {
        header_len,
        frame_len,
        object_type: (b2 >> 6).saturating_add(1),
        sampling_index: (b2 >> 2) & 0x0F,
        channel_config: ((b2 & 1) << 2) | (b3 >> 6),
    })
}

/// Strip a 7- or 9-byte ADTS header: returns the raw AAC payload and the
/// parsed header when `frame` starts with the syncword **and** the header's
/// frame length equals the buffer length; otherwise the frame is returned
/// unchanged (it is already raw).
#[must_use]
pub fn strip_adts(frame: &[u8]) -> (&[u8], Option<AdtsHeader>) {
    if let Some(header) = parse_adts_header(frame) {
        if header.frame_len == frame.len() {
            if let Some(payload) = frame.get(header.header_len..) {
                return (payload, Some(header));
            }
        }
    }
    (frame, None)
}

/// Build the 2-byte AAC-LC `AudioSpecificConfig` for a sample rate and
/// channel count. `None` when the rate is not an MPEG-4 table rate or the
/// channel count has no channel configuration (1..=6, or 8 as 7.1).
#[must_use]
pub fn aac_lc_audio_specific_config(sample_rate: u32, channels: u16) -> Option<Vec<u8>> {
    let index = SAMPLE_RATES.iter().position(|&rate| rate == sample_rate)?;
    let channel_config = match channels {
        1..=6 => u8::try_from(channels).ok()?,
        8 => 7,
        _ => return None,
    };
    Some(asc_bytes(2, u8::try_from(index).ok()?, channel_config))
}

/// The MPEG-4 audio object type of an `AudioSpecificConfig` (handles the
/// escape value 31), or `None` when it is too short.
#[must_use]
pub fn asc_object_type(asc: &[u8]) -> Option<u8> {
    let first = *asc.first()?;
    let object_type = first >> 3;
    if object_type != 31 {
        return Some(object_type);
    }
    let second = *asc.get(1)?;
    Some((((first & 0x07) << 3) | (second >> 5)).saturating_add(32))
}

/// The RFC 6381 codec string `mp4a.40.<object type>` of an
/// `AudioSpecificConfig` (`mp4a.40.2` for AAC-LC; also the fallback when the
/// config is empty).
#[must_use]
pub fn mp4a_codec_string(asc: &[u8]) -> String {
    format!("mp4a.40.{}", asc_object_type(asc).unwrap_or(2))
}

/// One box found by [`read_boxes`]: its type and its payload (the bytes after
/// the 8-byte header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRef<'a> {
    /// The four-character box type.
    pub fourcc: [u8; 4],
    /// The box payload (everything after `size` + `type`).
    pub payload: &'a [u8],
    /// Offset of the box's first byte (its size field) within the parsed
    /// buffer.
    pub offset: usize,
}

/// Walk the sibling boxes of `data` (a file, or the payload of a container
/// box). Sizes must nest exactly: a box that is shorter than its header, runs
/// past the end, or uses the 64-bit / to-end size forms is an error.
///
/// # Errors
/// Returns [`Error::Output`] describing the first malformed box.
pub fn read_boxes(data: &[u8]) -> Result<Vec<BoxRef<'_>>> {
    let mut boxes = Vec::new();
    let mut offset = 0usize;
    while offset < data.len() {
        let rest = data.get(offset..).unwrap_or_default();
        let header = rest.split_first_chunk::<8>().map(|(head, _)| *head);
        let Some([s0, s1, s2, s3, t0, t1, t2, t3]) = header else {
            return Err(Error::Output(format!(
                "truncated box header at offset {offset}"
            )));
        };
        let size = usize::try_from(u32::from_be_bytes([s0, s1, s2, s3])).unwrap_or(usize::MAX);
        let fourcc = [t0, t1, t2, t3];
        let Some(payload) = (size >= 8).then(|| rest.get(8..size)).flatten() else {
            return Err(Error::Output(format!(
                "box '{}' at offset {offset} has size {size} but {} bytes remain",
                String::from_utf8_lossy(&fourcc),
                rest.len()
            )));
        };
        boxes.push(BoxRef {
            fourcc,
            payload,
            offset,
        });
        offset = offset.saturating_add(size);
    }
    Ok(boxes)
}

/// Find the payload of the box at `path` (a chain of container box types
/// starting at the top level of `data`), following the first match at each
/// level. `None` when a level is missing or malformed.
#[must_use]
pub fn find_box<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let mut current = data;
    for fourcc in path {
        let boxes = read_boxes(current).ok()?;
        current = boxes.iter().find(|b| &&b.fourcc == fourcc)?.payload;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 27] = [
        0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
    ];
    const PPS: [u8; 6] = [0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

    fn be32(data: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
    }

    /// Every container nests exactly: walking a container's payload consumes
    /// it to the last byte, recursively.
    fn assert_nests(data: &[u8], containers: &[&[u8; 4]]) -> usize {
        let boxes = read_boxes(data).expect("boxes nest exactly");
        let mut count = boxes.len();
        for b in &boxes {
            if containers.contains(&&b.fourcc) {
                count += assert_nests(b.payload, containers);
            }
        }
        count
    }

    const CONTAINERS: [&[u8; 4]; 8] = [
        b"moov", b"trak", b"mdia", b"minf", b"dinf", b"stbl", b"mvex", b"moof",
    ];

    #[test]
    fn video_init_nests_and_carries_the_avcc() {
        let init = video_init_segment(30_000, 1920, 1080, &SPS, &PPS);
        let top = read_boxes(&init).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(&top[0].fourcc, b"ftyp");
        assert_eq!(top[0].payload, b"iso6\0\0\0\0iso6cmfcmp41");
        assert_eq!(&top[1].fourcc, b"moov");
        assert!(assert_nests(&init, &CONTAINERS) > 15);

        let mdhd = find_box(&init, &[b"moov", b"trak", b"mdia", b"mdhd"]).unwrap();
        assert_eq!(be32(mdhd, 12), 30_000);
        let tkhd = find_box(&init, &[b"moov", b"trak", b"tkhd"]).unwrap();
        assert_eq!(be32(tkhd, 12), 1, "track id");
        assert_eq!(be32(tkhd, 76) >> 16, 1920);
        assert_eq!(be32(tkhd, 80) >> 16, 1080);
        let hdlr = find_box(&init, &[b"moov", b"trak", b"mdia", b"hdlr"]).unwrap();
        assert_eq!(&hdlr[8..12], b"vide");
        let trex = find_box(&init, &[b"moov", b"mvex", b"trex"]).unwrap();
        assert_eq!((be32(trex, 4), be32(trex, 8)), (1, 1));

        let stsd = find_box(
            &init,
            &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"],
        )
        .unwrap();
        assert_eq!(be32(stsd, 4), 1);
        let entries = read_boxes(&stsd[8..]).unwrap();
        assert_eq!(&entries[0].fourcc, b"avc1");
        let entry = entries[0].payload;
        assert_eq!(u16::from_be_bytes([entry[24], entry[25]]), 1920);
        assert_eq!(u16::from_be_bytes([entry[26], entry[27]]), 1080);
        let inner = read_boxes(&entry[78..]).unwrap();
        assert_eq!(&inner[0].fourcc, b"avcC");
        let avcc = inner[0].payload;
        assert_eq!(&avcc[..6], &[1, 0x64, 0x00, 0x28, 0xFF, 0xE1]);
        assert_eq!(
            usize::from(u16::from_be_bytes([avcc[6], avcc[7]])),
            SPS.len()
        );
        assert_eq!(&avcc[8..8 + SPS.len()], &SPS);
        let at = 8 + SPS.len();
        assert_eq!(avcc[at], 1);
        assert_eq!(&avcc[at + 3..at + 3 + PPS.len()], &PPS);
        // High profile, 4:2:0 8-bit trailer.
        assert_eq!(&avcc[at + 3 + PPS.len()..], &[0xFD, 0xF8, 0xF8, 0]);
        assert_eq!(
            avcc_or_annexb_parameter_sets(avcc),
            Some((SPS.to_vec(), PPS.to_vec()))
        );
    }

    #[test]
    fn baseline_profile_has_no_high_trailer_and_444_is_parsed() {
        let baseline = [0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00];
        let record = avc_decoder_configuration_record(&baseline, &PPS);
        assert_eq!(record.len(), 6 + 2 + baseline.len() + 1 + 2 + PPS.len());
        // High 4:4:4 Predictive, sps_id ue(0)=1, chroma_format_idc ue(3)=00100,
        // separate_colour_plane 0, bit depths ue(2)=011 (10-bit) twice.
        let high444 = [0x67, 244, 0x00, 0x28, 0b1001_0000, 0b1101_1000];
        let record = avc_decoder_configuration_record(&high444, &PPS);
        assert_eq!(&record[record.len() - 4..], &[0xFF, 0xFA, 0xFA, 0]);
    }

    #[test]
    fn audio_init_carries_the_asc_in_the_esds() {
        let asc = aac_lc_audio_specific_config(48_000, 2).unwrap();
        assert_eq!(asc, vec![0x11, 0x90]);
        let init = audio_init_segment(48_000, 2, &asc, 192_000, 128_000);
        assert!(assert_nests(&init, &CONTAINERS) > 15);
        let mdhd = find_box(&init, &[b"moov", b"trak", b"mdia", b"mdhd"]).unwrap();
        assert_eq!(be32(mdhd, 12), 48_000);
        let hdlr = find_box(&init, &[b"moov", b"trak", b"mdia", b"hdlr"]).unwrap();
        assert_eq!(&hdlr[8..12], b"soun");
        assert!(find_box(&init, &[b"moov", b"trak", b"mdia", b"minf", b"smhd"]).is_some());
        let stsd = find_box(
            &init,
            &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"],
        )
        .unwrap();
        let entries = read_boxes(&stsd[8..]).unwrap();
        assert_eq!(&entries[0].fourcc, b"mp4a");
        let mp4a = entries[0].payload;
        assert_eq!(u16::from_be_bytes([mp4a[16], mp4a[17]]), 2, "channels");
        assert_eq!(be32(mp4a, 24) >> 16, 48_000);
        let inner = read_boxes(&mp4a[28..]).unwrap();
        assert_eq!(&inner[0].fourcc, b"esds");
        let esds = &inner[0].payload[4..];
        // ES_Descriptor(3) > DecoderConfig(4) > DecoderSpecificInfo(5).
        assert_eq!(esds[0], 0x03);
        assert_eq!(usize::from(esds[1]), esds.len() - 2);
        assert_eq!(esds[5], 0x04);
        assert_eq!(esds[7], 0x40, "object type indication");
        assert_eq!(esds[8], 0x15, "audio stream");
        assert_eq!(be32(esds, 12), 192_000);
        assert_eq!(be32(esds, 16), 128_000);
        assert_eq!(&esds[20..24], &[0x05, 2, 0x11, 0x90]);
        assert_eq!(&esds[24..], &[0x06, 1, 2]);
        assert_eq!(mp4a_codec_string(&asc), "mp4a.40.2");
    }

    fn sample(duration: u32, flags: u32, cts: i32, data: &[u8]) -> Sample {
        Sample {
            duration,
            flags,
            composition_offset: cts,
            data: data.to_vec(),
        }
    }

    #[test]
    fn media_segment_data_offset_points_at_the_mdat_payload() {
        let samples = [
            sample(1001, SAMPLE_FLAGS_SYNC, 0, &[1, 2, 3, 4, 5]),
            sample(1001, SAMPLE_FLAGS_NON_SYNC, 0, &[6, 7, 8]),
        ];
        let seg = media_segment(7, 0x1_0000_0001, &samples).unwrap();
        let top = read_boxes(&seg).unwrap();
        let kinds: Vec<&[u8; 4]> = top.iter().map(|b| &b.fourcc).collect();
        assert_eq!(kinds, [b"styp", b"moof", b"mdat"]);
        assert_eq!(top[0].payload, b"msdh\0\0\0\0msdhmsix");
        assert_nests(&seg, &[b"moof", b"traf"]);
        assert_eq!(top[2].payload, &[1, 2, 3, 4, 5, 6, 7, 8]);

        let mfhd = find_box(&seg, &[b"moof", b"mfhd"]).unwrap();
        assert_eq!(be32(mfhd, 4), 7);
        let tfhd = find_box(&seg, &[b"moof", b"traf", b"tfhd"]).unwrap();
        assert_eq!(be32(tfhd, 0), 0x02_0000);
        assert_eq!(be32(tfhd, 4), 1);
        let tfdt = find_box(&seg, &[b"moof", b"traf", b"tfdt"]).unwrap();
        assert_eq!(tfdt[0], 1, "tfdt version 1");
        assert_eq!(
            u64::from_be_bytes(tfdt[4..12].try_into().unwrap()),
            0x1_0000_0001
        );
        let trun = find_box(&seg, &[b"moof", b"traf", b"trun"]).unwrap();
        assert_eq!(
            be32(trun, 0),
            0x0000_0701,
            "v0: offset, duration, size, flags"
        );
        assert_eq!(be32(trun, 4), 2);
        let data_offset = usize::try_from(be32(trun, 8)).unwrap();
        let moof_at = top[1].offset;
        assert_eq!(moof_at + data_offset, top[2].offset + 8);
        assert_eq!(&seg[moof_at + data_offset..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            [be32(trun, 12), be32(trun, 16), be32(trun, 20)],
            [1001, 5, SAMPLE_FLAGS_SYNC]
        );
        assert_eq!(
            [be32(trun, 24), be32(trun, 28), be32(trun, 32)],
            [1001, 3, SAMPLE_FLAGS_NON_SYNC]
        );
        assert_eq!(trun.len(), 12 + 2 * 12);
    }

    #[test]
    fn composition_offsets_switch_the_trun_to_signed_version_one() {
        let samples = [
            sample(1001, SAMPLE_FLAGS_SYNC, 2002, &[1]),
            sample(1001, SAMPLE_FLAGS_NON_SYNC, -1001, &[2, 3]),
        ];
        let seg = media_segment(1, 0, &samples).unwrap();
        let top = read_boxes(&seg).unwrap();
        let trun = find_box(&seg, &[b"moof", b"traf", b"trun"]).unwrap();
        assert_eq!(be32(trun, 0), 0x0100_0F01);
        assert_eq!(be32(trun, 24), 2002);
        assert_eq!(i32::from_be_bytes(trun[40..44].try_into().unwrap()), -1001);
        let data_offset = usize::try_from(be32(trun, 8)).unwrap();
        assert_eq!(top[1].offset + data_offset, top[2].offset + 8);
        // An empty segment is still well-formed.
        let empty = media_segment(2, 9, &[]).unwrap();
        let top = read_boxes(&empty).unwrap();
        assert_eq!(top[2].payload, b"");
    }

    fn annex_b_key() -> Vec<u8> {
        let mut au = vec![0, 0, 0, 1, 0x09, 0xF0];
        au.extend_from_slice(&[0, 0, 0, 1]);
        au.extend_from_slice(&SPS);
        au.extend_from_slice(&[0, 0, 1]);
        au.extend_from_slice(&PPS);
        au.extend_from_slice(&[0, 0, 1, 0x0C, 0xFF, 0x80]);
        au.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x21]);
        au
    }

    #[test]
    fn annex_b_and_avcc_access_units_produce_the_same_sample() {
        let unit = h264_access_unit(&annex_b_key(), None).unwrap();
        assert_eq!(unit.framing, Some(NalFraming::AnnexB));
        assert!(unit.idr);
        assert_eq!(unit.sps.as_deref(), Some(&SPS[..]));
        assert_eq!(unit.pps.as_deref(), Some(&PPS[..]));
        assert_eq!(unit.data, [0, 0, 0, 5, 0x65, 0x88, 0x84, 0x00, 0x21]);

        // The same NAL units, AVCC framed.
        let key = annex_b_key();
        let nals = split_nal_units(&key, None).unwrap();
        assert_eq!(nals.len(), 5);
        let mut avcc = Vec::new();
        for nal in &nals {
            avcc.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
            avcc.extend_from_slice(nal);
        }
        let from_avcc = h264_access_unit(&avcc, None).unwrap();
        assert_eq!(from_avcc.framing, Some(NalFraming::Avcc));
        assert_eq!(from_avcc.data, unit.data);
        assert_eq!(from_avcc.sps, unit.sps);
        assert!(from_avcc.idr);

        let p = h264_access_unit(&[0, 0, 1, 0x41, 0x9A, 0x80], None).unwrap();
        assert!(!p.idr && p.sps.is_none());
        assert_eq!(p.data, [0, 0, 0, 3, 0x41, 0x9A, 0x80]);
        // Zero bytes after a NAL unit are `trailing_zero_8bits`, not payload
        // (an RBSP always ends with its stop bit, never with 0x00).
        let padded = h264_access_unit(&[0, 0, 1, 0x41, 0x9A, 0x80, 0, 0], None).unwrap();
        assert_eq!(padded.data, p.data);
        assert!(h264_access_unit(&[0x41, 0x9A, 0x00], None).is_err());
        assert!(h264_access_unit(&[], None).is_err());
    }

    #[test]
    fn an_avcc_unit_whose_first_nal_is_256_bytes_is_not_mistaken_for_annex_b() {
        // Length 0x0000_0100 starts with 00 00 01.
        let mut avcc = vec![0, 0, 1, 0];
        avcc.push(0x65);
        avcc.extend(std::iter::repeat_n(0xAB, 255));
        assert_eq!(detect_nal_framing(&avcc), Some(NalFraming::Avcc));
        let unit = h264_access_unit(&avcc, None).unwrap();
        assert!(unit.idr);
        assert_eq!(unit.data, avcc);
    }

    #[test]
    fn parameter_sets_from_extradata_in_both_forms() {
        let mut annex_b = vec![0, 0, 0, 1];
        annex_b.extend_from_slice(&SPS);
        annex_b.extend_from_slice(&[0, 0, 0, 1]);
        annex_b.extend_from_slice(&PPS);
        let expected = Some((SPS.to_vec(), PPS.to_vec()));
        assert_eq!(avcc_or_annexb_parameter_sets(&annex_b), expected);
        let record = avc_decoder_configuration_record(&SPS, &PPS);
        assert_eq!(avcc_or_annexb_parameter_sets(&record), expected);
        assert_eq!(avcc_or_annexb_parameter_sets(&[]), None);
        assert_eq!(avcc_or_annexb_parameter_sets(&[1, 2, 3]), None);
        assert_eq!(
            avcc_or_annexb_parameter_sets(&annex_b[..SPS.len() + 4]),
            None
        );
        assert_eq!(avc1_codec_string(&SPS), "avc1.640028");
        assert_eq!(avc1_codec_string(&[0x67, 0x4D, 0x40, 0x1F]), "avc1.4D401F");
    }

    #[test]
    fn adts_headers_are_stripped_only_when_the_length_matches() {
        // AAC-LC, 48 kHz (index 3), stereo, no CRC, frame length 7 + 4.
        let frame = [0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x7F, 0xFC, 9, 8, 7, 6];
        let (payload, header) = strip_adts(&frame);
        assert_eq!(payload, [9, 8, 7, 6]);
        let header = header.unwrap();
        assert_eq!((header.header_len, header.frame_len), (7, 11));
        assert_eq!(header.sample_rate(), Some(48_000));
        assert_eq!(header.audio_specific_config(), vec![0x11, 0x90]);
        // Raw frame that merely starts with the syncword: length mismatch.
        let raw = [0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x7F, 0xFC, 9, 8];
        assert_eq!(strip_adts(&raw), (&raw[..], None));
        // With CRC (protection_absent = 0): 9-byte header.
        let crc = [0xFF, 0xF0, 0x50, 0x40, 0x01, 0x5F, 0xFC, 0xAA, 0xBB, 1];
        let (payload, header) = strip_adts(&crc);
        assert_eq!(payload, [1]);
        let header = header.unwrap();
        assert_eq!(header.audio_specific_config(), vec![0x12, 0x08]);
        assert_eq!(
            aac_lc_audio_specific_config(44_100, 1),
            Some(vec![0x12, 0x08])
        );
        assert_eq!(aac_lc_audio_specific_config(47_999, 2), None);
        assert_eq!(asc_object_type(&[0x11, 0x90]), Some(2));
        assert_eq!(asc_object_type(&[]), None);
    }
}
