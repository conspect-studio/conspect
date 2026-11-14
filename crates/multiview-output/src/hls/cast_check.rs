//! A strict validator for a Cast packager directory.
//!
//! [`check_cast_dir`] reads a directory the way a player does: the master
//! playlist, both media playlists, every init segment and every listed media
//! segment, and reports everything that would make a player stall, reject
//! the stream or drift (a "mediastreamvalidator-style" check). It is pure
//! Rust and reads files only; the example `cast_validate` wraps it.
//!
//! What it checks is listed on [`check_cast_dir`]. Findings a player
//! tolerates are `warnings`; everything else is an `error`.
use std::collections::BTreeMap;
use std::path::Path;

use super::cmaf::{self, BoxRef};

/// Limits of [`check_cast_dir_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CastCheckOptions {
    /// The largest number of segments a media playlist may list (10).
    pub max_window: usize,
}

impl Default for CastCheckOptions {
    fn default() -> Self {
        Self { max_window: 10 }
    }
}

/// The result of [`check_cast_dir`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CastCheckReport {
    /// Violations: the directory is not a valid cast stream.
    pub errors: Vec<String>,
    /// Findings a player tolerates.
    pub warnings: Vec<String>,
    /// Segments listed by the video playlist.
    pub video_segments: usize,
    /// Segments listed by the audio playlist (0 without audio).
    pub audio_segments: usize,
    /// The master playlist's `CODECS` value.
    pub codecs: String,
    /// The video playlist's `EXT-X-TARGETDURATION`.
    pub target_duration: u64,
    /// The largest `EXTINF` of either playlist, in seconds.
    pub max_extinf: f64,
    /// The video playlist's `EXT-X-MEDIA-SEQUENCE`.
    pub media_sequence: u64,
    /// The video playlist's `EXT-X-DISCONTINUITY-SEQUENCE`.
    pub discontinuity_sequence: u64,
    /// `EXT-X-DISCONTINUITY` tags in the video playlist.
    pub discontinuities: usize,
    /// Whether the master lists an audio rendition.
    pub has_audio: bool,
    /// Whether the segments carry `EXT-X-PROGRAM-DATE-TIME`.
    pub has_program_date_time: bool,
    /// Start time of the first listed video segment, in seconds
    /// (`tfdt / timescale`).
    pub first_video_start: f64,
    /// Start time of the first listed audio segment, in seconds.
    pub first_audio_start: f64,
}

impl CastCheckReport {
    /// Whether the directory passed (no errors).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Compare two [`CastCheckReport`]s of the same directory taken at
/// different times: reports what a live playlist must never change between
/// snapshots (MEDIA-1's `EXT-X-TARGETDURATION`, and the codecs it
/// advertises). Cheap: no filesystem access, just the two already-computed
/// reports.
#[must_use]
pub fn check_stability(before: &CastCheckReport, after: &CastCheckReport) -> Vec<String> {
    let mut findings = Vec::new();
    if before.target_duration != 0
        && after.target_duration != 0
        && before.target_duration != after.target_duration
    {
        findings.push(format!(
            "EXT-X-TARGETDURATION changed across snapshots: {} -> {}",
            before.target_duration, after.target_duration
        ));
    }
    if !before.codecs.is_empty() && !after.codecs.is_empty() && before.codecs != after.codecs {
        findings.push(format!(
            "master.m3u8 CODECS changed across snapshots: \"{}\" -> \"{}\"",
            before.codecs, after.codecs
        ));
    }
    findings
}

/// Split an attribute list (`A=1,B="x,y",C=z`) into its pairs; quotes are
/// removed from quoted values.
fn parse_attributes(list: &str) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    let mut rest = list;
    while !rest.is_empty() {
        let Some((name, tail)) = rest.split_once('=') else {
            break;
        };
        let (value, tail) = match tail.strip_prefix('"') {
            Some(quoted) => match quoted.split_once('"') {
                Some((value, tail)) => (value, tail),
                None => (quoted, ""),
            },
            None => match tail.split_once(',') {
                Some((value, _)) => (value, tail.get(value.len()..).unwrap_or_default()),
                None => (tail, ""),
            },
        };
        attrs.insert(name.trim().to_owned(), value.to_owned());
        rest = tail.strip_prefix(',').unwrap_or(tail);
    }
    attrs
}

/// One segment entry of a media playlist.
#[derive(Debug, Clone)]
struct Entry {
    uri: String,
    extinf: f64,
    discontinuity: bool,
    /// `EXT-X-PROGRAM-DATE-TIME` in milliseconds past the Unix epoch.
    pdt_ms: Option<i64>,
    /// The `EXT-X-MAP` URI in force for this segment.
    map: Option<String>,
}

/// A parsed media playlist.
#[derive(Debug, Clone, Default)]
struct Playlist {
    version: Option<u64>,
    target_duration: Option<u64>,
    media_sequence: Option<u64>,
    discontinuity_sequence: u64,
    endlist: bool,
    entries: Vec<Entry>,
}

/// Days since 1970-01-01 of a proleptic Gregorian civil date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DDThh:mm:ss(.fff)?Z` into milliseconds past the Unix
/// epoch.
fn parse_program_date_time(text: &str) -> Option<i64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let mut clock = time.splitn(3, ':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let seconds = clock.next()?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second: i64 = whole.parse().ok()?;
    let mut millis = 0i64;
    let mut scale = 100i64;
    for digit in fraction.chars().take(3) {
        millis += i64::from(digit.to_digit(10)?) * scale;
        scale /= 10;
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

/// Parse a media playlist. Structural problems are pushed onto `errors`
/// (prefixed with `name`).
fn parse_media_playlist(name: &str, text: &str, errors: &mut Vec<String>) -> Playlist {
    let mut playlist = Playlist::default();
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    if lines.next() != Some("#EXTM3U") {
        errors.push(format!("{name}: does not start with #EXTM3U"));
    }
    let mut map: Option<String> = None;
    let mut extinf: Option<f64> = None;
    let mut discontinuity = false;
    let mut pdt_ms: Option<i64> = None;
    let number = |value: &str, tag: &str, errors: &mut Vec<String>| {
        let parsed = value.trim().parse::<u64>().ok();
        if parsed.is_none() {
            errors.push(format!("{name}: {tag} has a non-integer value '{value}'"));
        }
        parsed
    };
    for line in lines {
        if let Some(value) = line.strip_prefix("#EXT-X-VERSION:") {
            playlist.version = number(value, "EXT-X-VERSION", errors);
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            playlist.target_duration = number(value, "EXT-X-TARGETDURATION", errors);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            playlist.media_sequence = number(value, "EXT-X-MEDIA-SEQUENCE", errors);
        } else if let Some(value) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            playlist.discontinuity_sequence =
                number(value, "EXT-X-DISCONTINUITY-SEQUENCE", errors).unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            map = parse_attributes(value).remove("URI");
            if map.is_none() {
                errors.push(format!("{name}: EXT-X-MAP without URI"));
            }
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if line == "#EXT-X-ENDLIST" {
            playlist.endlist = true;
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pdt_ms = parse_program_date_time(value);
            if pdt_ms.is_none() {
                errors.push(format!(
                    "{name}: unparsable EXT-X-PROGRAM-DATE-TIME '{value}'"
                ));
            }
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let seconds = value.split(',').next().unwrap_or_default().trim();
            extinf = seconds.parse::<f64>().ok().filter(|v| v.is_finite());
            if extinf.is_none() {
                errors.push(format!("{name}: unparsable EXTINF '{value}'"));
            }
        } else if !line.starts_with('#') {
            match extinf.take() {
                Some(duration) => playlist.entries.push(Entry {
                    uri: line.to_owned(),
                    extinf: duration,
                    discontinuity,
                    pdt_ms: pdt_ms.take(),
                    map: map.clone(),
                }),
                None => errors.push(format!("{name}: segment '{line}' has no EXTINF")),
            }
            discontinuity = false;
            pdt_ms = None;
        }
    }
    playlist
}

/// Record `name: what` and return `None` (the caller gives up on the file).
fn report<T>(errors: &mut Vec<String>, name: &str, what: &str) -> Option<T> {
    errors.push(format!("{name}: {what}"));
    None
}

/// What an init segment declares.
#[derive(Debug, Clone, Default)]
struct InitInfo {
    timescale: u32,
    /// `avc1.PPCCLL` or `mp4a.40.N`.
    codec: String,
    video: bool,
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    let bytes = data.get(at..at.checked_add(8)?)?;
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

fn find<'a>(boxes: &[BoxRef<'a>], fourcc: [u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|b| b.fourcc == fourcc).map(|b| b.payload)
}

/// Read an MPEG-4 descriptor header at the start of `data`: `(tag, payload,
/// rest)`.
fn read_descriptor(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, mut rest) = data.split_first()?;
    let mut len = 0usize;
    for _ in 0..4 {
        let (&byte, tail) = rest.split_first()?;
        rest = tail;
        len = (len << 7) | usize::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            break;
        }
    }
    Some((tag, rest.get(..len)?, rest.get(len..)?))
}

/// The `AudioSpecificConfig` inside an `esds` payload (after the full-box
/// header) and its object type indication.
fn esds_config(esds: &[u8]) -> Option<(u8, &[u8])> {
    let (tag, es, _) = read_descriptor(esds.get(4..)?)?;
    if tag != 0x03 {
        return None;
    }
    // ES_ID (2) + flags (1); the optional fields are not used by our writer.
    let (tag, decoder_config, _) = read_descriptor(es.get(3..)?)?;
    if tag != 0x04 {
        return None;
    }
    let object_type_indication = *decoder_config.first()?;
    let (tag, asc, _) = read_descriptor(decoder_config.get(13..)?)?;
    (tag == 0x05).then_some((object_type_indication, asc))
}

/// Parse an init segment (`ftyp` + `moov`).
fn parse_init(name: &str, bytes: &[u8], errors: &mut Vec<String>) -> Option<InitInfo> {
    let mut fail = |what: &str| report(errors, name, what);
    let top = match cmaf::read_boxes(bytes) {
        Ok(top) => top,
        Err(e) => return fail(&format!("malformed boxes: {e}")),
    };
    if top.first().map(|b| &b.fourcc) != Some(b"ftyp") {
        return fail("does not start with ftyp");
    }
    if cmaf::find_box(bytes, &[b"moov", b"mvex", b"trex"]).is_none() {
        return fail("no moov/mvex/trex (not a fragmented-MP4 init segment)");
    }
    let Some(mdhd) = cmaf::find_box(bytes, &[b"moov", b"trak", b"mdia", b"mdhd"]) else {
        return fail("no moov/trak/mdia/mdhd");
    };
    let version = mdhd.first().copied().unwrap_or(0);
    let timescale = be_u32(mdhd, if version == 1 { 20 } else { 12 }).unwrap_or(0);
    if timescale == 0 {
        return fail("mdhd timescale is zero");
    }
    let stsd = cmaf::find_box(
        bytes,
        &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"],
    );
    let entries = stsd
        .and_then(|stsd| stsd.get(8..))
        .and_then(|entries| cmaf::read_boxes(entries).ok());
    let Some(entry) = entries.as_ref().and_then(|entries| entries.first()) else {
        return fail("no sample entry in stsd");
    };
    match &entry.fourcc {
        b"avc1" => {
            let inner = entry
                .payload
                .get(78..)
                .and_then(|b| cmaf::read_boxes(b).ok());
            let Some(avcc) = inner.as_ref().and_then(|boxes| find(boxes, *b"avcC")) else {
                return fail("avc1 sample entry without avcC");
            };
            let Some((sps, _pps)) = cmaf::avcc_or_annexb_parameter_sets(avcc) else {
                return fail("avcC holds no SPS and PPS");
            };
            if avcc.get(1..4) != sps.get(1..4) {
                return fail("avcC profile/level bytes differ from its SPS");
            }
            Some(InitInfo {
                timescale,
                codec: cmaf::avc1_codec_string(&sps),
                video: true,
            })
        }
        b"mp4a" => {
            let inner = entry
                .payload
                .get(28..)
                .and_then(|b| cmaf::read_boxes(b).ok());
            let Some(esds) = inner.as_ref().and_then(|boxes| find(boxes, *b"esds")) else {
                return fail("mp4a sample entry without esds");
            };
            let Some((object_type_indication, asc)) = esds_config(esds) else {
                return fail("esds holds no AudioSpecificConfig");
            };
            if object_type_indication != 0x40 {
                return fail("esds object type indication is not 0x40 (MPEG-4 audio)");
            }
            let Some(object_type) = cmaf::asc_object_type(asc) else {
                return fail("empty AudioSpecificConfig");
            };
            Some(InitInfo {
                timescale,
                codec: format!("mp4a.40.{object_type}"),
                video: false,
            })
        }
        other => fail(&format!(
            "unsupported sample entry '{}'",
            String::from_utf8_lossy(other)
        )),
    }
}

/// What a media segment declares.
#[derive(Debug, Clone, Default)]
struct SegmentInfo {
    /// `baseMediaDecodeTime`.
    tfdt: u64,
    /// Sum of the `trun` sample durations.
    duration: u64,
    samples: usize,
    /// Composition offset of the first sample (its pts minus the `tfdt`).
    first_offset: i64,
}

/// Check that an AVCC sample's NAL lengths walk exactly to its end; returns
/// the type of its first VCL NAL (1..=5), if any.
fn walk_avcc_sample(sample: &[u8]) -> Result<Option<u8>, String> {
    let mut rest = sample;
    let mut first_vcl = None;
    while !rest.is_empty() {
        let Some((len_bytes, tail)) = rest.split_first_chunk::<4>() else {
            return Err("a NAL length field is truncated".to_owned());
        };
        let len = usize::try_from(u32::from_be_bytes(*len_bytes)).unwrap_or(usize::MAX);
        let Some(nal) = tail.get(..len).filter(|nal| !nal.is_empty()) else {
            return Err(format!(
                "a NAL length of {len} does not fit the {} bytes left in the sample",
                tail.len()
            ));
        };
        let kind = cmaf::nal_unit_type(nal);
        if first_vcl.is_none() && (1..=5).contains(&kind) {
            first_vcl = Some(kind);
        }
        rest = tail.get(len..).unwrap_or_default();
    }
    Ok(first_vcl)
}

/// The single movie fragment of a media segment.
struct Fragment<'a> {
    /// `baseMediaDecodeTime`.
    tfdt: u64,
    /// The `trun` payload (from its version/flags word).
    trun: &'a [u8],
    /// File offset the `trun` data offset points at.
    data_start: usize,
    /// File offsets of the `mdat` payload.
    payload_start: usize,
    payload_end: usize,
}

/// Find the one `moof` / `traf` / `mdat` of a media segment and check the
/// fragment headers.
fn locate_fragment<'a>(
    name: &str,
    bytes: &'a [u8],
    errors: &mut Vec<String>,
) -> Option<Fragment<'a>> {
    let top = match cmaf::read_boxes(bytes) {
        Ok(top) => top,
        Err(e) => return report(errors, name, &format!("malformed boxes: {e}")),
    };
    if top.first().map(|b| &b.fourcc) != Some(b"styp") {
        return report(errors, name, "does not start with styp");
    }
    let moofs: Vec<&BoxRef<'_>> = top.iter().filter(|b| &b.fourcc == b"moof").collect();
    let mdats: Vec<&BoxRef<'_>> = top.iter().filter(|b| &b.fourcc == b"mdat").collect();
    let (&[moof], &[mdat]) = (moofs.as_slice(), mdats.as_slice()) else {
        return report(
            errors,
            name,
            &format!(
                "expected exactly one moof and one mdat, found {} and {}",
                moofs.len(),
                mdats.len()
            ),
        );
    };
    let Ok(moof_boxes) = cmaf::read_boxes(moof.payload) else {
        return report(errors, name, "malformed moof");
    };
    let trafs: Vec<&BoxRef<'_>> = moof_boxes.iter().filter(|b| &b.fourcc == b"traf").collect();
    let &[traf] = trafs.as_slice() else {
        return report(
            errors,
            name,
            &format!("expected exactly one traf, found {}", trafs.len()),
        );
    };
    let Ok(traf_boxes) = cmaf::read_boxes(traf.payload) else {
        return report(errors, name, "malformed traf");
    };
    let Some(tfhd_flags) = find(&traf_boxes, *b"tfhd").and_then(|tfhd| be_u32(tfhd, 0)) else {
        return report(errors, name, "no tfhd");
    };
    if tfhd_flags & 0x02_0000 == 0 {
        return report(errors, name, "tfhd does not set default-base-is-moof");
    }
    if tfhd_flags & 0x00_0001 != 0 {
        return report(errors, name, "tfhd carries a base-data-offset");
    }
    let Some(tfdt) = find(&traf_boxes, *b"tfdt") else {
        return report(errors, name, "no tfdt");
    };
    let tfdt = if tfdt.first() == Some(&1) {
        be_u64(tfdt, 4)
    } else {
        be_u32(tfdt, 4).map(u64::from)
    };
    let Some(tfdt) = tfdt else {
        return report(errors, name, "truncated tfdt");
    };
    let Some(trun) = find(&traf_boxes, *b"trun") else {
        return report(errors, name, "no trun");
    };
    let Some(flags) = be_u32(trun, 0) else {
        return report(errors, name, "truncated trun");
    };
    if flags & 0x0001 == 0 || flags & 0x0100 == 0 || flags & 0x0200 == 0 {
        return report(
            errors,
            name,
            "trun lacks data-offset, sample-duration or sample-size",
        );
    }
    let Some(data_offset) = be_u32(trun, 8).and_then(|v| usize::try_from(v).ok()) else {
        return report(errors, name, "truncated trun");
    };
    let payload_start = mdat.offset.saturating_add(8);
    let payload_end = payload_start.saturating_add(mdat.payload.len());
    let data_start = moof.offset.saturating_add(data_offset);
    if data_start < payload_start || data_start > payload_end {
        return report(
            errors,
            name,
            &format!("trun data offset {data_offset} points outside the mdat payload"),
        );
    }
    Some(Fragment {
        tfdt,
        trun,
        data_start,
        payload_start,
        payload_end,
    })
}

/// Parse and check one media segment (`styp` + `moof` + `mdat`).
fn parse_segment(
    name: &str,
    bytes: &[u8],
    video: bool,
    errors: &mut Vec<String>,
) -> Option<SegmentInfo> {
    let Fragment {
        tfdt,
        trun,
        data_start,
        payload_start,
        payload_end,
    } = locate_fragment(name, bytes, errors)?;
    let (Some(header), Some(count)) = (be_u32(trun, 0), be_u32(trun, 4)) else {
        return report(errors, name, "truncated trun");
    };
    let (version, flags) = (header >> 24, header & 0x00FF_FFFF);
    let mut at = 12usize.saturating_add(if flags & 0x0004 == 0 { 0 } else { 4 });
    let mut info = SegmentInfo {
        tfdt,
        ..SegmentInfo::default()
    };
    let mut cursor = data_start;
    let mut total_size = 0usize;
    for index in 0..count {
        let (Some(duration), Some(size)) = (be_u32(trun, at), be_u32(trun, at.saturating_add(4)))
        else {
            return report(
                errors,
                name,
                &format!("trun is truncated at sample {index}"),
            );
        };
        at = at.saturating_add(8);
        let sample_flags = if flags & 0x0400 == 0 {
            None
        } else {
            at = at.saturating_add(4);
            be_u32(trun, at.saturating_sub(4))
        };
        if flags & 0x0800 != 0 {
            let Some(raw) = be_u32(trun, at) else {
                return report(
                    errors,
                    name,
                    &format!("trun is truncated at sample {index}"),
                );
            };
            at = at.saturating_add(4);
            // pts = dts + offset must not precede the dts.
            let offset = if version >= 1 {
                i64::from(i32::from_be_bytes(raw.to_be_bytes()))
            } else {
                i64::from(raw)
            };
            if offset < 0 {
                errors.push(format!(
                    "{name}: sample {index} has a negative composition offset (pts < dts)"
                ));
            }
            if index == 0 {
                info.first_offset = offset;
            }
        }
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let end = cursor.saturating_add(size);
        if video {
            match bytes.get(cursor..end).map(walk_avcc_sample) {
                Some(Ok(first_vcl)) if index == 0 => {
                    let sync = sample_flags.is_some_and(|f| f & 0x0001_0000 == 0);
                    if !sync {
                        errors.push(format!("{name}: the first sample is not a sync sample"));
                    }
                    if first_vcl != Some(5) {
                        errors.push(format!(
                            "{name}: the first sample does not start with an IDR slice (first \
                             VCL NAL type {})",
                            first_vcl.map_or_else(|| "none".to_owned(), |k| k.to_string())
                        ));
                    }
                }
                Some(Err(what)) => errors.push(format!("{name}: sample {index}: {what}")),
                Some(Ok(_)) | None => {}
            }
        }
        if duration == 0 {
            errors.push(format!("{name}: sample {index} has a zero duration"));
        }
        cursor = end;
        total_size = total_size.saturating_add(size);
        info.duration = info.duration.saturating_add(u64::from(duration));
        info.samples = info.samples.saturating_add(1);
    }
    if cursor != payload_end || data_start != payload_start {
        errors.push(format!(
            "{name}: the trun sample sizes sum to {total_size} bytes but the mdat payload is {} \
             bytes",
            payload_end.saturating_sub(payload_start)
        ));
    }
    if info.samples == 0 {
        errors.push(format!("{name}: the segment holds no samples"));
    }
    Some(info)
}

/// One listed segment after its file was checked.
#[derive(Debug, Clone)]
struct Listed {
    sequence: u64,
    /// `EXT-X-DISCONTINUITY-SEQUENCE` plus the discontinuities up to and
    /// including this segment.
    discontinuity_counter: u64,
    pdt_ms: Option<i64>,
    /// Presentation start in seconds, when the segment parsed.
    start: Option<f64>,
}

/// A checked media playlist.
#[derive(Debug, Clone, Default)]
struct Rendition {
    playlist: Playlist,
    listed: Vec<Listed>,
    /// The codec of the init in force for the last listed segment.
    codec: Option<String>,
    max_extinf: f64,
}

/// A URI must name a file directly in the directory.
fn plain_file_name(uri: &str) -> bool {
    !uri.is_empty() && !uri.contains(['/', '\\', '?', '#']) && uri != "." && uri != ".."
}

fn ratio(ticks: u64, timescale: u32) -> f64 {
    multiview_core::time::Rational::new(
        i64::try_from(ticks).unwrap_or(i64::MAX),
        i64::from(timescale.max(1)),
    )
    .as_f64()
}

fn millis(ms: i64) -> f64 {
    multiview_core::time::Rational::new(ms, 1000).as_f64()
}

/// The playlist-level requirements of a cast media playlist.
fn check_playlist_header(
    name: &str,
    playlist: &Playlist,
    options: CastCheckOptions,
    errors: &mut Vec<String>,
) {
    if playlist.version.is_none_or(|version| version < 6) {
        errors.push(format!(
            "{name}: EXT-X-VERSION must be at least 6 for EXT-X-MAP, found {:?}",
            playlist.version
        ));
    }
    if playlist.media_sequence.is_none() {
        errors.push(format!("{name}: no EXT-X-MEDIA-SEQUENCE"));
    }
    if playlist.endlist {
        errors.push(format!(
            "{name}: a live playlist must not carry EXT-X-ENDLIST"
        ));
    }
    if playlist.entries.is_empty() {
        errors.push(format!("{name}: lists no segments"));
    }
    if playlist.entries.len() > options.max_window {
        errors.push(format!(
            "{name}: lists {} segments, more than the window of {}",
            playlist.entries.len(),
            options.max_window
        ));
    }
    let with_pdt = playlist
        .entries
        .iter()
        .filter(|e| e.pdt_ms.is_some())
        .count();
    if with_pdt != 0 && with_pdt != playlist.entries.len() {
        errors.push(format!(
            "{name}: EXT-X-PROGRAM-DATE-TIME on {with_pdt} of {} segments (must be all or none)",
            playlist.entries.len()
        ));
    }
    if playlist.target_duration.is_none() {
        errors.push(format!("{name}: no EXT-X-TARGETDURATION"));
    }
}

/// Read and parse the init segment `map` names.
fn load_init(dir: &Path, name: &str, map: &str, errors: &mut Vec<String>) -> Option<InitInfo> {
    if !plain_file_name(map) {
        errors.push(format!("{name}: EXT-X-MAP URI '{map}' is not a file here"));
        return None;
    }
    match std::fs::read(dir.join(map)) {
        Ok(bytes) => parse_init(map, &bytes, errors),
        Err(e) => {
            errors.push(format!("{name}: init segment {map} cannot be read: {e}"));
            None
        }
    }
}

/// Read and check the media segment `uri` names.
fn load_segment(
    dir: &Path,
    name: &str,
    uri: &str,
    video: bool,
    errors: &mut Vec<String>,
) -> Option<SegmentInfo> {
    if !plain_file_name(uri) {
        errors.push(format!("{name}: segment URI '{uri}' is not a file here"));
        return None;
    }
    match std::fs::read(dir.join(uri)) {
        Ok(bytes) => parse_segment(uri, &bytes, video, errors),
        Err(e) => {
            errors.push(format!("{name}: segment {uri} cannot be read: {e}"));
            None
        }
    }
}

/// Read and check one media playlist with its init and media segments.
fn check_rendition(
    dir: &Path,
    name: &str,
    video: bool,
    options: CastCheckOptions,
    errors: &mut Vec<String>,
) -> Option<Rendition> {
    let text = match std::fs::read_to_string(dir.join(name)) {
        Ok(text) => text,
        Err(e) => {
            errors.push(format!("{name}: cannot be read: {e}"));
            return None;
        }
    };
    let playlist = parse_media_playlist(name, &text, errors);
    check_playlist_header(name, &playlist, options, errors);
    let target = playlist.target_duration;

    let mut rendition = Rendition::default();
    let mut inits: BTreeMap<String, Option<InitInfo>> = BTreeMap::new();
    let mut counter = playlist.discontinuity_sequence;
    // (tfdt + duration, timescale) and (PDT, EXTINF) of the previous segment.
    let mut previous: Option<(u64, u32)> = None;
    let mut previous_pdt: Option<(i64, f64)> = None;
    let mut sequence = playlist.media_sequence.unwrap_or(0);
    for entry in &playlist.entries {
        let uri = entry.uri.as_str();
        if entry.discontinuity {
            counter = counter.saturating_add(1);
        }
        rendition.max_extinf = rendition.max_extinf.max(entry.extinf);
        if target.is_some_and(|target| entry.extinf.round() > ratio(target, 1)) {
            errors.push(format!(
                "{name}: {uri}: EXTINF {} exceeds EXT-X-TARGETDURATION {}",
                entry.extinf,
                target.unwrap_or(0)
            ));
        }
        if let (Some((before, extinf)), Some(now)) = (previous_pdt, entry.pdt_ms) {
            let delta = millis(now.saturating_sub(before));
            if !entry.discontinuity && (delta - extinf).abs() > 0.002 {
                errors.push(format!(
                    "{name}: {uri}: EXT-X-PROGRAM-DATE-TIME advanced {delta:.3} s over a \
                     segment of {extinf:.6} s"
                ));
            }
        }
        previous_pdt = entry.pdt_ms.map(|pdt| (pdt, entry.extinf));

        let init = match entry.map.as_deref() {
            None => {
                errors.push(format!("{name}: {uri}: no EXT-X-MAP in force"));
                None
            }
            Some(map) => inits
                .entry(map.to_owned())
                .or_insert_with(|| load_init(dir, name, map, errors))
                .clone(),
        };
        if let Some(init) = &init {
            if init.video != video {
                errors.push(format!(
                    "{name}: {uri}: the init segment is not a{} track",
                    if video { " video" } else { "n audio" }
                ));
            }
            rendition.codec = Some(init.codec.clone());
        }
        let info = load_segment(dir, name, uri, video, errors);
        let mut start = None;
        if let (Some(info), Some(init)) = (&info, &init) {
            let duration = ratio(info.duration, init.timescale);
            if (duration - entry.extinf).abs() > 0.001 {
                errors.push(format!(
                    "{name}: {uri}: EXTINF {} but the samples last {duration:.6} s",
                    entry.extinf
                ));
            }
            if let Some((expected, timescale)) = previous {
                if !entry.discontinuity && (timescale != init.timescale || expected != info.tfdt) {
                    errors.push(format!(
                        "{name}: {uri}: tfdt {} does not continue the previous segment (expected \
                         {expected}) and no EXT-X-DISCONTINUITY is declared",
                        info.tfdt
                    ));
                }
            }
            previous = Some((info.tfdt.saturating_add(info.duration), init.timescale));
            let first_pts = i64::try_from(info.tfdt)
                .unwrap_or(i64::MAX)
                .saturating_add(info.first_offset);
            start = Some(ratio(u64::try_from(first_pts).unwrap_or(0), init.timescale));
        } else {
            previous = None;
        }
        rendition.listed.push(Listed {
            sequence,
            discontinuity_counter: counter,
            pdt_ms: entry.pdt_ms,
            start,
        });
        sequence = sequence.saturating_add(1);
    }
    rendition.playlist = playlist;
    Some(rendition)
}

/// What the master playlist declares.
#[derive(Debug, Clone, Default)]
struct Master {
    codecs: String,
    video_uri: Option<String>,
    audio_uri: Option<String>,
}

fn parse_master(text: &str, errors: &mut Vec<String>) -> Master {
    const NAME: &str = "master.m3u8";
    let mut master = Master::default();
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    if lines.next() != Some("#EXTM3U") {
        errors.push(format!("{NAME}: does not start with #EXTM3U"));
    }
    let mut stream_inf: Option<BTreeMap<String, String>> = None;
    let mut audio_group: Option<String> = None;
    let mut variants = 0usize;
    for line in lines {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = parse_attributes(value);
            if attrs.get("TYPE").map(String::as_str) == Some("AUDIO") {
                audio_group = attrs.get("GROUP-ID").cloned();
                master.audio_uri = attrs.get("URI").cloned();
                if master.audio_uri.is_none() {
                    errors.push(format!("{NAME}: the audio EXT-X-MEDIA has no URI"));
                }
            }
        } else if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            stream_inf = Some(parse_attributes(value));
        } else if !line.starts_with('#') {
            let Some(attrs) = stream_inf.take() else {
                errors.push(format!("{NAME}: URI '{line}' without EXT-X-STREAM-INF"));
                continue;
            };
            variants = variants.saturating_add(1);
            for required in ["BANDWIDTH", "CODECS", "RESOLUTION", "FRAME-RATE"] {
                if attrs.get(required).is_none_or(String::is_empty) {
                    errors.push(format!("{NAME}: EXT-X-STREAM-INF has no {required}"));
                }
            }
            if attrs
                .get("BANDWIDTH")
                .is_some_and(|v| v.parse::<u64>().map_or(true, |bits| bits == 0))
            {
                errors.push(format!("{NAME}: BANDWIDTH is not a positive integer"));
            }
            if attrs.get("AUDIO") != audio_group.as_ref() {
                errors.push(format!(
                    "{NAME}: the variant's AUDIO group {:?} does not match the EXT-X-MEDIA \
                     GROUP-ID {audio_group:?}",
                    attrs.get("AUDIO")
                ));
            }
            master.codecs = attrs.get("CODECS").cloned().unwrap_or_default();
            master.video_uri = Some(line.to_owned());
        }
    }
    if variants != 1 {
        errors.push(format!(
            "{NAME}: expected one variant stream, found {variants}"
        ));
    }
    master
}

/// Check a cast directory with the default limits (window of 10). See
/// [`check_cast_dir_with`].
#[must_use]
pub fn check_cast_dir(dir: &Path) -> CastCheckReport {
    check_cast_dir_with(dir, CastCheckOptions::default())
}

/// Read `dir` as a player would and report every violation.
///
/// Checked: the master playlist (`BANDWIDTH`, `CODECS`, `RESOLUTION`,
/// `FRAME-RATE`; every URI is a file in the directory); each media playlist
/// (version at least 6, `EXT-X-MAP`, target duration, media sequence, no
/// `EXT-X-ENDLIST`, the window, every `EXTINF` within the target duration,
/// program-date-time on all segments or none and advancing by the segment
/// durations); the init segments (`CODECS` equals what `avcC` / `esds`
/// declare, a non-zero timescale, `mvex/trex`); every listed media segment
/// (one `moof` + `mdat`, one `traf`, default-base-is-moof, `tfdt`, a data
/// offset into the `mdat`, sample sizes summing to the `mdat` payload, a
/// first video sample that is a sync sample starting with an IDR, NAL
/// lengths that walk each sample exactly, `EXTINF` equal to the sample
/// durations, `tfdt` continuous unless a discontinuity is declared); and
/// across renditions: equal sequence ranges (one trailing segment of
/// slack), equal discontinuity sequences and segment starts within 25 ms.
#[must_use]
pub fn check_cast_dir_with(dir: &Path, options: CastCheckOptions) -> CastCheckReport {
    let mut report = CastCheckReport::default();
    let mut errors = Vec::new();
    let master = match std::fs::read_to_string(dir.join("master.m3u8")) {
        Ok(text) => parse_master(&text, &mut errors),
        Err(e) => {
            errors.push(format!("master.m3u8: cannot be read: {e}"));
            report.errors = errors;
            return report;
        }
    };
    report.codecs.clone_from(&master.codecs);
    report.has_audio = master.audio_uri.is_some();
    let check = |uri: Option<&String>, video: bool, errors: &mut Vec<String>| {
        let uri = uri?;
        if !plain_file_name(uri) {
            errors.push(format!("master.m3u8: URI '{uri}' is not a file here"));
            return None;
        }
        check_rendition(dir, uri, video, options, errors)
    };
    let video = check(master.video_uri.as_ref(), true, &mut errors);
    let audio = check(master.audio_uri.as_ref(), false, &mut errors);

    let mut expected_codecs = Vec::new();
    if let Some(video) = &video {
        report.video_segments = video.listed.len();
        report.target_duration = video.playlist.target_duration.unwrap_or(0);
        report.media_sequence = video.playlist.media_sequence.unwrap_or(0);
        report.discontinuity_sequence = video.playlist.discontinuity_sequence;
        report.discontinuities = video
            .playlist
            .entries
            .iter()
            .filter(|e| e.discontinuity)
            .count();
        report.has_program_date_time = video.listed.iter().any(|s| s.pdt_ms.is_some());
        report.max_extinf = video.max_extinf;
        report.first_video_start = video.listed.first().and_then(|s| s.start).unwrap_or(0.0);
        expected_codecs.extend(video.codec.clone());
    }
    if let Some(audio) = &audio {
        report.audio_segments = audio.listed.len();
        report.max_extinf = report.max_extinf.max(audio.max_extinf);
        report.first_audio_start = audio.listed.first().and_then(|s| s.start).unwrap_or(0.0);
        expected_codecs.extend(audio.codec.clone());
    }
    let expected_codecs = expected_codecs.join(",");
    if !expected_codecs.is_empty() && !master.codecs.is_empty() && master.codecs != expected_codecs
    {
        errors.push(format!(
            "master.m3u8: CODECS \"{}\" but the init segments declare \"{expected_codecs}\"",
            master.codecs
        ));
    }
    if let (Some(video), Some(audio)) = (&video, &audio) {
        cross_check(video, audio, &mut errors, &mut report.warnings);
    }
    report.errors = errors;
    report
}

/// Compare the video and the audio rendition: the same sequence range (one
/// trailing segment of slack while a publish is in flight), and for every
/// common sequence number the same discontinuity count and a start time
/// within 25 ms.
fn cross_check(
    video: &Rendition,
    audio: &Rendition,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let range = |rendition: &Rendition| {
        let first = rendition.listed.first().map(|s| s.sequence)?;
        let last = rendition.listed.last().map(|s| s.sequence)?;
        Some((first, last))
    };
    let (Some((v_first, v_last)), Some((a_first, a_last))) = (range(video), range(audio)) else {
        return;
    };
    if v_first.abs_diff(a_first) > 1 || v_last.abs_diff(a_last) > 1 {
        errors.push(format!(
            "video.m3u8 lists media sequences {v_first}..={v_last} but audio.m3u8 lists \
             {a_first}..={a_last}"
        ));
    } else if v_first != a_first || v_last != a_last {
        warnings.push(format!(
            "the playlists are one segment apart (video {v_first}..={v_last}, audio \
             {a_first}..={a_last}): a publish was in flight"
        ));
    }
    if video.playlist.target_duration != audio.playlist.target_duration {
        warnings.push(format!(
            "EXT-X-TARGETDURATION differs: video {:?}, audio {:?}",
            video.playlist.target_duration, audio.playlist.target_duration
        ));
    }
    let mut common = 0usize;
    for v in &video.listed {
        let Some(a) = audio.listed.iter().find(|a| a.sequence == v.sequence) else {
            continue;
        };
        common = common.saturating_add(1);
        if v.discontinuity_counter != a.discontinuity_counter {
            errors.push(format!(
                "media sequence {}: the video discontinuity sequence is {} but the audio one is \
                 {}",
                v.sequence, v.discontinuity_counter, a.discontinuity_counter
            ));
        }
        if let (Some(video_start), Some(audio_start)) = (v.start, a.start) {
            if (video_start - audio_start).abs() > 0.025 {
                errors.push(format!(
                    "media sequence {}: video starts at {video_start:.6} s but audio at \
                     {audio_start:.6} s (more than 25 ms apart)",
                    v.sequence
                ));
            }
        }
        if let (Some(video_pdt), Some(audio_pdt)) = (v.pdt_ms, a.pdt_ms) {
            if video_pdt.abs_diff(audio_pdt) > 25 {
                errors.push(format!(
                    "media sequence {}: the video and audio EXT-X-PROGRAM-DATE-TIME are {} ms \
                     apart",
                    v.sequence,
                    video_pdt.abs_diff(audio_pdt)
                ));
            }
        }
    }
    if common == 0 {
        errors.push("video.m3u8 and audio.m3u8 share no media sequence number".to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_lists_keep_commas_inside_quotes() {
        let attrs = parse_attributes(
            "BANDWIDTH=6192000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,AUDIO=\"aud\"",
        );
        assert_eq!(attrs["BANDWIDTH"], "6192000");
        assert_eq!(attrs["CODECS"], "avc1.640028,mp4a.40.2");
        assert_eq!(attrs["RESOLUTION"], "1920x1080");
        assert_eq!(attrs["AUDIO"], "aud");
        assert_eq!(attrs.len(), 4);
    }

    #[test]
    fn program_date_time_round_trips_through_the_formatter() {
        for unix_ms in [0_i64, 1_767_323_045_010, 951_782_400_000, 4_102_444_799_999] {
            let text = super::super::format_program_date_time(unix_ms * 1_000_000);
            assert_eq!(parse_program_date_time(&text), Some(unix_ms), "{text}");
        }
        assert_eq!(
            parse_program_date_time("2026-01-02T03:04:05Z"),
            Some(1_767_323_045_000)
        );
        assert_eq!(parse_program_date_time("2026-13-02T03:04:05Z"), None);
        assert_eq!(parse_program_date_time("yesterday"), None);
    }

    #[test]
    fn media_playlists_parse_maps_discontinuities_and_dates() {
        let text = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n\
                    #EXT-X-MEDIA-SEQUENCE:4\n#EXT-X-DISCONTINUITY-SEQUENCE:1\n\
                    #EXT-X-MAP:URI=\"init-v-1-0.mp4\"\n\
                    #EXT-X-PROGRAM-DATE-TIME:2026-01-02T03:04:05.000Z\n#EXTINF:2.002000,\na.m4s\n\
                    #EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init-v-1-1.mp4\"\n#EXTINF:1.001000,\n\
                    b.m4s\nstray.m4s\n";
        let mut errors = Vec::new();
        let playlist = parse_media_playlist("video.m3u8", text, &mut errors);
        assert_eq!(errors, ["video.m3u8: segment 'stray.m4s' has no EXTINF"]);
        assert_eq!(
            (
                playlist.version,
                playlist.target_duration,
                playlist.media_sequence
            ),
            (Some(7), Some(2), Some(4))
        );
        assert_eq!(playlist.discontinuity_sequence, 1);
        assert!(!playlist.endlist);
        let [a, b] = playlist.entries.as_slice() else {
            panic!("two entries expected");
        };
        assert_eq!(
            (a.uri.as_str(), a.map.as_deref()),
            ("a.m4s", Some("init-v-1-0.mp4"))
        );
        assert_eq!(a.pdt_ms, Some(1_767_323_045_000));
        assert!(!a.discontinuity && b.discontinuity);
        assert_eq!(
            (b.uri.as_str(), b.map.as_deref()),
            ("b.m4s", Some("init-v-1-1.mp4"))
        );
        assert_eq!(b.pdt_ms, None);
    }

    #[test]
    fn a_missing_directory_is_one_clear_error() {
        let report = check_cast_dir(Path::new("/nonexistent/cast-dir"));
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].starts_with("master.m3u8: cannot be read"));
        assert!(!report.is_ok());
    }

    #[test]
    fn check_stability_flags_a_target_duration_or_codecs_change_but_not_zero_values() {
        let mut before = CastCheckReport {
            target_duration: 4,
            codecs: "avc1.640028,mp4a.40.2".to_owned(),
            ..CastCheckReport::default()
        };
        let same = before.clone();
        assert_eq!(super::check_stability(&before, &same), Vec::<String>::new());

        let mut after = before.clone();
        after.target_duration = 6;
        let findings = super::check_stability(&before, &after);
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].contains("EXT-X-TARGETDURATION changed"),
            "{findings:?}"
        );

        after = before.clone();
        after.codecs = "avc1.4D401F,mp4a.40.2".to_owned();
        let findings = super::check_stability(&before, &after);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("CODECS changed"), "{findings:?}");

        // A report with a zero (unread) field never triggers a finding.
        before.target_duration = 0;
        assert_eq!(super::check_stability(&before, &after), {
            let mut only_codecs = Vec::new();
            if before.codecs != after.codecs {
                only_codecs.push(format!(
                    "master.m3u8 CODECS changed across snapshots: \"{}\" -> \"{}\"",
                    before.codecs, after.codecs
                ));
            }
            only_codecs
        });
    }
}
