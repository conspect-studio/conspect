//! Read-only HLS evidence for the event journal: every cue-shaped tag with its
//! playlist position, and the first PTS of a TS segment (the anchor that ties
//! `EXT-X-PROGRAM-DATE-TIME` to the source PTS timeline).
//!
//! Nothing here drives a splice boundary. Segment URIs lose their query,
//! fragment and userinfo, because CDN tokens must not reach the journal.
use crate::hls::parse_rfc3339_ns;
use crate::scte::decode::{decode_section, DecodedSection};
use base64::Engine as _;
use serde::Serialize;
use std::collections::BTreeMap;

const MAX_TAGS: usize = 1024;
const MAX_TAG_BYTES: usize = 16_384;
const TS_PACKET: usize = 188;
const MAX_SCANNED_PACKETS: usize = 8192;
const CUE_TAGS: [&str; 9] = [
    "#EXT-X-CUE-OUT-CONT",
    "#EXT-X-CUE-OUT",
    "#EXT-X-CUE-IN",
    "#EXT-X-CUE",
    "#EXT-X-DATERANGE",
    "#EXT-X-ASSET",
    "#EXT-OATCLS-SCTE35",
    "#EXT-X-SCTE35",
    "#EXT-X-SPLICEPOINT-SCTE35",
];

/// One cue-shaped tag and where the playlist put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalTag {
    /// The raw tag line.
    pub line: String,
    /// Media sequence of the segment the tag precedes.
    pub media_sequence: Option<u64>,
    /// That segment's URI, without query, fragment or userinfo.
    pub segment_uri: Option<String>,
    /// That segment's program date-time, as written or extrapolated.
    pub pdt_ns: Option<i64>,
}

/// Everything the journal needs from one playlist load.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlaylistJournal {
    /// Cue tags in document order, at most 1024.
    pub tags: Vec<JournalTag>,
    /// `(media sequence, program date-time ns)` per dated segment.
    pub segment_pdt: Vec<(u64, i64)>,
    /// Tags omitted by the size or count bound.
    pub dropped: usize,
}

/// Remove the query, fragment and userinfo from a segment URI.
#[must_use]
pub fn sanitize_uri(uri: &str) -> String {
    let end = uri.find(['?', '#']).unwrap_or(uri.len());
    let path = uri.get(..end).unwrap_or(uri);
    if let Some((scheme, rest)) = path.split_once("://") {
        let (authority, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let host = authority.rsplit('@').next().unwrap_or(authority);
        return format!("{scheme}://{host}{tail}");
    }
    path.to_owned()
}

fn is_cue_tag(line: &str) -> bool {
    CUE_TAGS.iter().any(|tag| {
        line.strip_prefix(tag)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
    })
}

fn extinf_ns(line: &str) -> Option<i64> {
    let value = line.strip_prefix("#EXTINF:")?.split(',').next()?.trim();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let seconds: i64 = whole.parse().ok()?;
    let digits: String = fraction
        .chars()
        .chain("000000000".chars())
        .take(9)
        .collect();
    seconds
        .checked_mul(1_000_000_000)?
        .checked_add(digits.parse().ok()?)
}

/// Scan one media playlist. Tags bind to the segment that follows them; a
/// trailing tag keeps the position of the next segment that would follow.
#[must_use]
pub fn scan_playlist(text: &str) -> PlaylistJournal {
    let mut out = PlaylistJournal::default();
    let mut sequence = 0_u64;
    let mut pdt: Option<i64> = None;
    let mut duration = 0_i64;
    let mut pending_from = 0_usize;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = value.trim().parse().unwrap_or(sequence);
        } else if let Some(value) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            pdt = parse_rfc3339_ns(value).or(pdt);
        } else if line.starts_with("#EXTINF:") {
            duration = extinf_ns(line).unwrap_or(0);
        } else if is_cue_tag(line) {
            if out.tags.len() >= MAX_TAGS || line.len() > MAX_TAG_BYTES {
                out.dropped += 1;
            } else {
                out.tags.push(JournalTag {
                    line: line.to_owned(),
                    media_sequence: Some(sequence),
                    segment_uri: None,
                    pdt_ns: pdt,
                });
            }
        } else if !line.starts_with('#') {
            let uri = sanitize_uri(line);
            for tag in out.tags.iter_mut().skip(pending_from) {
                tag.segment_uri = Some(uri.clone());
                tag.pdt_ns = pdt;
            }
            pending_from = out.tags.len();
            if let Some(value) = pdt {
                out.segment_pdt.push((sequence, value));
            }
            pdt = pdt.and_then(|value| value.checked_add(duration));
            sequence = sequence.saturating_add(1);
        }
    }
    out
}

/// One SCTE-35 payload found inside a tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TagSection {
    /// The attribute or tag that carried it.
    pub carrier: String,
    /// The decode, when the payload was a readable section.
    pub section: Option<DecodedSection>,
    /// Why it could not be decoded.
    pub error: Option<String>,
}

/// One decoded cue tag.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecodedTag {
    /// Tag name, without the leading `#`.
    pub tag: String,
    /// Attributes, quote-stripped.
    pub attributes: BTreeMap<String, String>,
    /// The tag's value when it is not an attribute list.
    pub value: Option<String>,
    /// Declared duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Elapsed time in milliseconds (`CUE-OUT-CONT`).
    pub elapsed_ms: Option<u64>,
    /// `START-DATE` as nanoseconds past the Unix epoch.
    #[serde(skip)]
    pub start_ns: Option<i64>,
    /// Embedded SCTE-35.
    pub scte35: Vec<TagSection>,
}

fn seconds_to_ms(value: &str) -> Option<u64> {
    let (whole, fraction) = value.trim().split_once('.').unwrap_or((value.trim(), ""));
    let seconds: u64 = whole.parse().ok()?;
    let digits: String = fraction.chars().chain("000".chars()).take(3).collect();
    seconds.checked_mul(1000)?.checked_add(digits.parse().ok()?)
}

fn parse_attributes(list: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut quoted = false;
    let mut start = 0;
    for (index, c) in list
        .char_indices()
        .chain(std::iter::once((list.len(), ',')))
    {
        if c == '"' {
            quoted = !quoted;
        } else if c == ',' && !quoted {
            if let Some((key, value)) = list.get(start..index).and_then(|p| p.split_once('=')) {
                out.insert(
                    key.trim().to_owned(),
                    value.trim().trim_matches('"').to_owned(),
                );
            }
            start = index.saturating_add(1);
        }
    }
    out
}

fn from_hex(value: &str) -> Option<Vec<u8>> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.is_empty() || digits.len() % 2 != 0 {
        return None;
    }
    digits
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect()
}

fn payload_bytes(value: &str) -> Option<Vec<u8>> {
    let trimmed = value.trim();
    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        return from_hex(trimmed);
    }
    base64::engine::general_purpose::STANDARD
        .decode(trimmed)
        .ok()
        .or_else(|| from_hex(trimmed))
}

fn tag_section(carrier: &str, value: &str) -> TagSection {
    let decoded = payload_bytes(value)
        .ok_or_else(|| "payload is neither hex nor base64".to_owned())
        .and_then(|bytes| decode_section(&bytes));
    let (section, error) = match decoded {
        Ok(section) => (Some(section), None),
        Err(error) => (None, Some(error)),
    };
    TagSection {
        carrier: carrier.to_owned(),
        section,
        error,
    }
}

/// Decode one cue tag line. Unknown shapes keep their raw value.
#[must_use]
pub fn decode_tag(line: &str) -> DecodedTag {
    let body = line.strip_prefix('#').unwrap_or(line);
    let (tag, value) = body.split_once(':').unwrap_or((body, ""));
    let attribute_list = value.contains('=');
    let attributes = if attribute_list {
        parse_attributes(value)
    } else {
        BTreeMap::new()
    };
    let attr = |name: &str| {
        attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    let mut duration_ms = ["DURATION", "PLANNED-DURATION", "Duration"]
        .iter()
        .find_map(|name| attr(name).and_then(seconds_to_ms));
    let mut elapsed_ms = ["ELAPSEDTIME", "ELAPSED"]
        .iter()
        .find_map(|name| attr(name).and_then(seconds_to_ms));
    if !attribute_list && !value.is_empty() {
        match value.split_once('/') {
            Some((elapsed, total)) if tag == "EXT-X-CUE-OUT-CONT" => {
                elapsed_ms = seconds_to_ms(elapsed);
                duration_ms = seconds_to_ms(total);
            }
            _ if tag == "EXT-X-CUE-OUT" => duration_ms = seconds_to_ms(value),
            _ => {}
        }
    }
    let mut scte35: Vec<TagSection> = attributes
        .iter()
        .filter(|(key, _)| {
            ["SCTE35-CMD", "SCTE35-OUT", "SCTE35-IN", "SCTE35", "CUE"]
                .iter()
                .any(|name| key.eq_ignore_ascii_case(name))
        })
        .map(|(key, value)| tag_section(key, value))
        .collect();
    if tag == "EXT-OATCLS-SCTE35" && !attribute_list && !value.is_empty() {
        scte35.push(tag_section(tag, value));
    }
    DecodedTag {
        tag: tag.to_owned(),
        start_ns: attr("START-DATE").and_then(parse_rfc3339_ns),
        value: (!attribute_list && !value.is_empty()).then(|| value.to_owned()),
        attributes,
        duration_ms,
        elapsed_ms,
        scte35,
    }
}

fn pes_pts(payload: &[u8]) -> Option<(u8, u64)> {
    let &[0, 0, 1, stream_id, _, _, _, flags, _, p0, p1, p2, p3, p4, ..] = payload else {
        return None;
    };
    if flags & 0x80 == 0 || !matches!(stream_id, 0xC0..=0xEF) {
        return None;
    }
    let pts = (u64::from(p0 >> 1) & 0x07) << 30
        | u64::from(p1) << 22
        | u64::from(p2 >> 1) << 15
        | u64::from(p3) << 7
        | u64::from(p4 >> 1);
    Some((stream_id, pts))
}

/// PIDs that carry markers, read from a segment's own PAT and PMT.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataPids {
    /// SCTE-35 (`stream_type` 0x86).
    pub scte35: Vec<u16>,
    /// Timed ID3 (`stream_type` 0x15).
    pub id3: Vec<u16>,
}

fn psi_section(segment: &[u8], pid: u16) -> Option<&[u8]> {
    segment
        .chunks_exact(TS_PACKET)
        .take(MAX_SCANNED_PACKETS)
        .find_map(|packet| {
            let &[0x47, b1, b2, b3, ..] = packet else {
                return None;
            };
            let this = (u16::from(b1 & 0x1F) << 8) | u16::from(b2);
            if this != pid || b1 & 0x40 == 0 || b3 & 0x10 == 0 {
                return None;
            }
            let start = if b3 & 0x20 == 0 {
                4
            } else {
                5 + usize::from(*packet.get(4)?)
            };
            let pointer = usize::from(*packet.get(start)?);
            let section = packet.get(start + 1 + pointer..)?;
            let &[_, l1, l2, ..] = section else {
                return None;
            };
            let length = (usize::from(l1 & 0x0F) << 8 | usize::from(l2)) + 3;
            // Multi-packet tables are rare for a PAT/PMT; skip rather than assemble.
            section.get(..length)
        })
}

/// The marker PIDs of a TS segment. Empty when the tables are absent or
/// span packets. libav reports these PIDs as 0 behind its HLS demuxer.
#[must_use]
pub fn metadata_pids(segment: &[u8]) -> MetadataPids {
    use crate::mpegts::pmt::StreamType;
    let mut out = MetadataPids::default();
    let Some(pat) = psi_section(segment, 0).and_then(|s| crate::mpegts::pat::Pat::parse(s).ok())
    else {
        return out;
    };
    for program in pat.programs.iter().filter(|p| !p.is_network()) {
        let Some(pmt) =
            psi_section(segment, program.pid).and_then(|s| crate::mpegts::pmt::Pmt::parse(s).ok())
        else {
            continue;
        };
        for stream in &pmt.streams {
            match stream.stream_type {
                StreamType::Scte35 => out.scte35.push(stream.pid),
                StreamType::Other(0x15) => out.id3.push(stream.pid),
                _ => {}
            }
        }
    }
    out
}

/// The first video PES PTS of a TS segment, else its first audio PES PTS.
#[must_use]
pub fn first_pes_pts(segment: &[u8]) -> Option<u64> {
    let mut audio = None;
    for packet in segment.chunks_exact(TS_PACKET).take(MAX_SCANNED_PACKETS) {
        let &[0x47, b1, _, b3, ..] = packet else {
            return audio;
        };
        if b1 & 0x40 == 0 || b3 & 0x10 == 0 {
            continue;
        }
        let start = if b3 & 0x20 == 0 {
            4
        } else {
            5 + usize::from(*packet.get(4)?)
        };
        match packet.get(start..).and_then(pes_pts) {
            Some((0xE0..=0xEF, pts)) => return Some(pts),
            Some((_, pts)) => audio = audio.or(Some(pts)),
            None => {}
        }
    }
    audio
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYLIST: &str = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:100\n\
#EXT-X-PROGRAM-DATE-TIME:2026-11-03T21:14:58.000Z\n#EXTINF:6.000,\nhttps://u:p@cdn.example/a/1.ts?token=secret\n\
#EXT-X-CUE-OUT:30\n#EXTINF:6.000,\n2.ts?token=secret#f\n\
#EXT-X-CUE-OUT-CONT:6/30\n#EXTINF:6.000,\n3.ts\n#EXT-X-CUE-IN\n";

    #[test]
    fn tags_bind_to_the_following_segment_and_extrapolate_pdt() {
        let scan = scan_playlist(PLAYLIST);
        assert_eq!(scan.tags.len(), 3);
        let out = &scan.tags[0];
        assert_eq!(out.line, "#EXT-X-CUE-OUT:30");
        assert_eq!(out.media_sequence, Some(101));
        assert_eq!(out.segment_uri.as_deref(), Some("2.ts"));
        let first = scan.segment_pdt[0].1;
        assert_eq!(out.pdt_ns, Some(first + 6_000_000_000));
        assert_eq!(scan.tags[2].media_sequence, Some(103));
        assert_eq!(scan.tags[2].segment_uri, None);
        assert_eq!(scan.segment_pdt.len(), 3);
    }

    #[test]
    fn segment_uris_lose_tokens_and_credentials() {
        assert_eq!(
            sanitize_uri("https://u:p@cdn.example/a/1.ts?token=secret"),
            "https://cdn.example/a/1.ts"
        );
        assert_eq!(sanitize_uri("a/2.ts#frag"), "a/2.ts");
    }

    #[test]
    fn cue_tags_decode_durations() {
        assert_eq!(decode_tag("#EXT-X-CUE-OUT:30.5").duration_ms, Some(30_500));
        assert_eq!(
            decode_tag("#EXT-X-CUE-OUT:DURATION=12").duration_ms,
            Some(12_000)
        );
        let cont = decode_tag("#EXT-X-CUE-OUT-CONT:6.5/30");
        assert_eq!(
            (cont.elapsed_ms, cont.duration_ms),
            (Some(6500), Some(30_000))
        );
        let cont = decode_tag("#EXT-X-CUE-OUT-CONT:ElapsedTime=4,Duration=30,SCTE35=/DAvAAAA");
        assert_eq!(
            (cont.elapsed_ms, cont.duration_ms),
            (Some(4000), Some(30_000))
        );
        assert_eq!(decode_tag("#EXT-X-CUE-IN").tag, "EXT-X-CUE-IN");
    }

    #[test]
    fn daterange_scte_hex_decodes_as_a_section() {
        let line = "#EXT-X-DATERANGE:ID=\"b\",START-DATE=\"2026-11-03T21:15:00Z\",\
PLANNED-DURATION=30,SCTE35-OUT=0xFC302F000000000000FFFFF0140548000\
08F7FEFFE7369C02EFE0052CCF500000000000A0008435545490000013562DBA30A";
        let tag = decode_tag(line);
        assert_eq!(tag.duration_ms, Some(30_000));
        assert!(tag.start_ns.is_some());
        let section = tag.scte35[0].section.as_ref().unwrap();
        assert!(section.crc_ok);
        assert_eq!(section.splice_command_name, "splice_insert");
        assert_eq!(
            decode_tag("#EXT-X-DATERANGE:ID=\"b\",SCTE35-IN=0xZZ").scte35[0].section,
            None
        );
    }

    fn ts_packet(stream_id: u8, pts: u64) -> Vec<u8> {
        let mut p = vec![
            0x47, 0x41, 0x00, 0x10, 0, 0, 1, stream_id, 0, 0, 0x80, 0x80, 5,
        ];
        p.push(0x21 | (u8::try_from((pts >> 30) & 7).unwrap() << 1));
        p.push(u8::try_from((pts >> 22) & 0xFF).unwrap());
        p.push(1 | (u8::try_from((pts >> 15) & 0x7F).unwrap() << 1));
        p.push(u8::try_from((pts >> 7) & 0xFF).unwrap());
        p.push(1 | (u8::try_from(pts & 0x7F).unwrap() << 1));
        p.resize(TS_PACKET, 0xFF);
        p
    }

    fn psi(pid: u16, table: &[u8]) -> Vec<u8> {
        let mut section = table.to_vec();
        let length = u16::try_from(section.len() + 4 - 3).unwrap();
        section[1] = 0xB0 | u8::try_from(length >> 8).unwrap();
        section[2] = u8::try_from(length & 0xFF).unwrap();
        let crc = crate::mpegts::crc::crc32_mpeg2(&section);
        section.extend_from_slice(&crc.to_be_bytes());
        let mut packet = vec![
            0x47,
            0x40 | u8::try_from(pid >> 8).unwrap(),
            u8::try_from(pid & 0xFF).unwrap(),
            0x10,
            0,
        ];
        packet.extend_from_slice(&section);
        packet.resize(TS_PACKET, 0xFF);
        packet
    }

    #[test]
    fn marker_pids_come_from_the_segment_pmt() {
        let pat = psi(0, &[0x00, 0, 0, 0, 1, 0xC1, 0, 0, 0, 1, 0xF0, 0x00]);
        let pmt = psi(
            0x1000,
            &[
                0x02, 0, 0, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0x00, //
                0x1B, 0xE1, 0x00, 0xF0, 0x00, // H.264 on 0x100
                0x86, 0xE1, 0xE3, 0xF0, 0x00, // SCTE-35 on 0x1E3
                0x15, 0xE1, 0xE4, 0xF0, 0x00, // timed ID3 on 0x1E4
            ],
        );
        let pids = metadata_pids(&[pat, pmt].concat());
        assert_eq!(pids.scte35, vec![0x1E3]);
        assert_eq!(pids.id3, vec![0x1E4]);
        assert_eq!(metadata_pids(&[0_u8; 188]), MetadataPids::default());
    }

    #[test]
    fn first_pts_prefers_video_and_reads_all_33_bits() {
        let pts = (1_u64 << 32) + 12_345;
        let mut segment = ts_packet(0xC0, 7);
        segment.extend(ts_packet(0xE0, pts));
        assert_eq!(first_pes_pts(&segment), Some(pts));
        assert_eq!(first_pes_pts(&ts_packet(0xC0, 7)), Some(7));
        assert_eq!(first_pes_pts(&[0_u8; 188]), None);
    }
}
