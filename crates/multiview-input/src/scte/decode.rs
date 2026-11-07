//! Complete, best-effort SCTE-35 decode for the event journal.
//!
//! [`super::splice35`] fails closed because its output can drive automation.
//! This module is the opposite: it is read-only evidence for a human, so it
//! keeps everything it can read from a section, reports a failed CRC or a
//! truncated tail beside the partial result, and never feeds a splice boundary.
//! Field names follow SCTE-35 2023r1. All values are integers or strings; time
//! fields carry the wire 90 kHz ticks plus a derived millisecond value.
use serde::Serialize;

/// The 33-bit PTS modulus.
pub const PTS_MODULUS: u64 = 1 << 33;
const PTS_MASK: u64 = PTS_MODULUS - 1;
const GPS_EPOCH_UNIX_SECONDS: u64 = 315_964_800;

/// `(a + b) mod 2^33`, the splice-point rule `pts_time + pts_adjustment`.
#[must_use]
pub const fn pts_add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b) & PTS_MASK
}

/// Shortest signed distance from `from` to `to` on the 33-bit circle, in ticks.
#[must_use]
pub fn pts_delta(to: u64, from: u64) -> i64 {
    let forward = to.wrapping_sub(from) & PTS_MASK;
    let forward = i64::try_from(forward).unwrap_or(0);
    let modulus = i64::try_from(PTS_MODULUS).unwrap_or(i64::MAX);
    if forward >= modulus / 2 {
        forward - modulus
    } else {
        forward
    }
}

/// Whole milliseconds in `ticks` of the 90 kHz clock.
#[must_use]
pub const fn ticks_to_ms(ticks: u64) -> u64 {
    ticks / 90
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn printable(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty() && bytes.iter().all(|b| (0x20..=0x7e).contains(b)))
        .then(|| bytes.iter().copied().map(char::from).collect())
}

fn text_or_hex(bytes: &[u8]) -> String {
    printable(bytes).unwrap_or_else(|| format!("0x{}", hex(bytes)))
}

/// MSB-first reader; every read past the end is `None`, never a panic.
struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Bits<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn read(&mut self, width: usize) -> Option<u64> {
        let mut value = 0_u64;
        for _ in 0..width {
            let byte = *self.bytes.get(self.pos / 8)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            value = (value << 1) | u64::from(bit);
            self.pos += 1;
        }
        Some(value)
    }
    fn u8(&mut self, width: usize) -> Option<u8> {
        u8::try_from(self.read(width)?).ok()
    }
    fn u16(&mut self, width: usize) -> Option<u16> {
        u16::try_from(self.read(width)?).ok()
    }
    fn u32(&mut self, width: usize) -> Option<u32> {
        u32::try_from(self.read(width)?).ok()
    }
    fn flag(&mut self) -> Option<bool> {
        Some(self.read(1)? != 0)
    }
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        if self.pos % 8 != 0 {
            return None;
        }
        let start = self.pos / 8;
        let slice = self.bytes.get(start..start.checked_add(count)?)?;
        self.pos += count * 8;
        Some(slice)
    }
    fn rest(&mut self) -> &'a [u8] {
        let start = self.pos.div_ceil(8).min(self.bytes.len());
        self.pos = self.bytes.len() * 8;
        self.bytes.get(start..).unwrap_or_default()
    }
}

/// `break_duration()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BreakDuration {
    /// Whether the splicer returns automatically at the end of the break.
    pub auto_return: bool,
    /// Wire duration in 90 kHz ticks.
    pub duration: u64,
    /// Derived duration in milliseconds.
    pub duration_ms: u64,
}

/// One component of a component-level `splice_insert`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpliceComponent {
    /// Elementary stream component tag.
    pub component_tag: u8,
    /// Component splice time in ticks, absent for an immediate splice.
    pub pts_time: Option<u64>,
}

/// `splice_insert()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // reason: these are the spec's independent wire flags.
pub struct SpliceInsertCommand {
    /// Splice event identity.
    pub splice_event_id: u32,
    /// `splice_event_cancel_indicator`.
    pub splice_event_cancel: bool,
    /// Leaving the network feed (true) or returning to it (false).
    pub out_of_network: Option<bool>,
    /// Program splice (true) or component splice (false).
    pub program_splice: Option<bool>,
    /// `splice_immediate_flag`.
    pub splice_immediate: Option<bool>,
    /// `event_id_compliance_flag`.
    pub event_id_compliance: Option<bool>,
    /// Program `splice_time` in ticks.
    pub pts_time: Option<u64>,
    /// Per-component splice times.
    pub components: Vec<SpliceComponent>,
    /// Declared break duration.
    pub break_duration: Option<BreakDuration>,
    /// `unique_program_id`.
    pub unique_program_id: Option<u16>,
    /// `avail_num`.
    pub avail_num: Option<u8>,
    /// `avails_expected`.
    pub avails_expected: Option<u8>,
}

/// One event of a `splice_schedule`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScheduledSplice {
    /// Splice event identity.
    pub splice_event_id: u32,
    /// `splice_event_cancel_indicator`.
    pub splice_event_cancel: bool,
    /// Leaving the network feed (true) or returning to it (false).
    pub out_of_network: Option<bool>,
    /// Wire `utc_splice_time`, GPS seconds.
    pub utc_splice_time: Option<u32>,
    /// That instant as Unix seconds (GPS epoch offset only; leap seconds ignored).
    pub utc_splice_time_unix: Option<u64>,
    /// Declared break duration.
    pub break_duration: Option<BreakDuration>,
    /// `unique_program_id`.
    pub unique_program_id: Option<u16>,
    /// `avail_num`.
    pub avail_num: Option<u8>,
    /// `avails_expected`.
    pub avails_expected: Option<u8>,
}

/// The decoded splice command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// `splice_null()`.
    SpliceNull,
    /// `splice_schedule()`.
    SpliceSchedule {
        /// Scheduled events.
        events: Vec<ScheduledSplice>,
    },
    /// `splice_insert()`.
    SpliceInsert(SpliceInsertCommand),
    /// `time_signal()`.
    TimeSignal {
        /// `splice_time` in ticks; absent means "immediate".
        pts_time: Option<u64>,
    },
    /// `bandwidth_reservation()`.
    BandwidthReservation,
    /// `private_command()`.
    PrivateCommand {
        /// `identifier`, as hex.
        identifier: String,
        /// Private bytes, as hex.
        private_bytes: String,
    },
    /// A reserved command type, or a command hidden by encryption.
    Opaque {
        /// Raw command bytes, as hex.
        raw: String,
    },
}

/// One `segmentation_upid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Upid {
    /// `segmentation_upid_type`.
    pub upid_type: u8,
    /// Table name of that type.
    pub upid_type_name: &'static str,
    /// Decoded display form.
    pub value: String,
    /// Exact wire bytes, as hex.
    pub raw: String,
    /// Inner identifiers of a MID.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nested: Vec<Upid>,
}

/// Delivery restriction flags, present when `delivery_not_restricted_flag` is 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeliveryRestrictions {
    /// `web_delivery_allowed_flag`.
    pub web_delivery_allowed: bool,
    /// `no_regional_blackout_flag`.
    pub no_regional_blackout: bool,
    /// `archive_allowed_flag`.
    pub archive_allowed: bool,
    /// `device_restrictions`.
    pub device_restrictions: u8,
    /// Table name of `device_restrictions`.
    pub device_restrictions_name: &'static str,
}

/// One component offset of a component-level segmentation descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SegmentationComponent {
    /// Elementary stream component tag.
    pub component_tag: u8,
    /// `pts_offset` in ticks.
    pub pts_offset: u64,
}

/// `segmentation_descriptor()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Segmentation {
    /// `segmentation_event_id`.
    pub segmentation_event_id: u32,
    /// `segmentation_event_cancel_indicator`.
    pub segmentation_event_cancel: bool,
    /// Raw `segmentation_event_id_compliance_indicator` bit (0 means compliant).
    pub segmentation_event_id_compliance_indicator: bool,
    /// `program_segmentation_flag`.
    pub program_segmentation: Option<bool>,
    /// `delivery_not_restricted_flag`.
    pub delivery_not_restricted: Option<bool>,
    /// Restrictions, when delivery is restricted.
    pub delivery_restrictions: Option<DeliveryRestrictions>,
    /// Component offsets of a component-level descriptor.
    pub components: Vec<SegmentationComponent>,
    /// `segmentation_duration` in ticks.
    pub segmentation_duration: Option<u64>,
    /// Derived duration in milliseconds.
    pub segmentation_duration_ms: Option<u64>,
    /// Decoded `segmentation_upid`.
    pub upid: Option<Upid>,
    /// `segmentation_type_id`.
    pub segmentation_type_id: Option<u8>,
    /// Table name of `segmentation_type_id`.
    pub segmentation_type_name: Option<String>,
    /// `segment_num`.
    pub segment_num: Option<u8>,
    /// `segments_expected`.
    pub segments_expected: Option<u8>,
    /// `sub_segment_num`.
    pub sub_segment_num: Option<u8>,
    /// `sub_segments_expected`.
    pub sub_segments_expected: Option<u8>,
}

/// One audio component of an `audio_descriptor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioComponent {
    /// Elementary stream component tag.
    pub component_tag: u8,
    /// ISO 639 language code.
    pub iso_code: String,
    /// `Bit_Stream_Mode`.
    pub bit_stream_mode: u8,
    /// `Num_Channels`.
    pub num_channels: u8,
    /// `Full_Srvc_Audio`.
    pub full_srvc_audio: bool,
}

/// The body of one splice descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "descriptor", rename_all = "snake_case")]
pub enum DescriptorBody {
    /// `avail_descriptor()`.
    Avail {
        /// `provider_avail_id`.
        provider_avail_id: u32,
    },
    /// `DTMF_descriptor()`.
    Dtmf {
        /// `preroll`, in tenths of a second.
        preroll: u8,
        /// DTMF characters.
        dtmf: String,
    },
    /// `segmentation_descriptor()`.
    Segmentation(Box<Segmentation>),
    /// `time_descriptor()`.
    Time {
        /// `TAI_seconds`.
        tai_seconds: u64,
        /// `TAI_ns`.
        tai_ns: u32,
        /// `UTC_offset`.
        utc_offset: u16,
    },
    /// `audio_descriptor()`.
    Audio {
        /// Audio components.
        components: Vec<AudioComponent>,
    },
    /// A private or truncated descriptor.
    Opaque {
        /// Raw descriptor body, as hex.
        raw: String,
    },
}

/// One entry of the descriptor loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Descriptor {
    /// `splice_descriptor_tag`.
    pub tag: u8,
    /// Four-character identifier, normally `CUEI`.
    pub identifier: String,
    /// Decoded body.
    #[serde(flatten)]
    pub body: DescriptorBody,
}

/// The complete decode of one `splice_info_section`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecodedSection {
    /// `table_id`, always `0xFC`.
    pub table_id: u8,
    /// `sap_type`.
    pub sap_type: u8,
    /// `section_length`.
    pub section_length: u16,
    /// `protocol_version`.
    pub protocol_version: u8,
    /// `encrypted_packet`.
    pub encrypted_packet: bool,
    /// `encryption_algorithm`.
    pub encryption_algorithm: u8,
    /// `pts_adjustment` in ticks.
    pub pts_adjustment: u64,
    /// `cw_index`.
    pub cw_index: u8,
    /// `tier`.
    pub tier: u16,
    /// `splice_command_length`.
    pub splice_command_length: u16,
    /// `splice_command_type`.
    pub splice_command_type: u8,
    /// Table name of `splice_command_type`.
    pub splice_command_name: &'static str,
    /// Decoded command.
    pub splice_command: Command,
    /// Decoded descriptor loop.
    pub descriptors: Vec<Descriptor>,
    /// Carried `CRC_32`, as `0x`-prefixed hex.
    pub crc_32: String,
    /// Whether the carried CRC matches the section.
    pub crc_ok: bool,
    /// What stopped the decode early, if anything.
    pub parse_error: Option<String>,
}

impl DecodedSection {
    /// Segmentation descriptors in wire order.
    pub fn segmentations(&self) -> impl Iterator<Item = &Segmentation> {
        self.descriptors.iter().filter_map(|d| match &d.body {
            DescriptorBody::Segmentation(s) => Some(s.as_ref()),
            _ => None,
        })
    }

    /// The first segmentation descriptor that is not a cancellation.
    #[must_use]
    pub fn primary_segmentation(&self) -> Option<&Segmentation> {
        self.segmentations()
            .find(|s| !s.segmentation_event_cancel)
            .or_else(|| self.segmentations().next())
    }

    /// The command's program `pts_time`, before `pts_adjustment`.
    #[must_use]
    pub const fn pts_time(&self) -> Option<u64> {
        match &self.splice_command {
            Command::SpliceInsert(insert) => insert.pts_time,
            Command::TimeSignal { pts_time } => *pts_time,
            _ => None,
        }
    }

    /// The splice-point PTS: `(pts_time + pts_adjustment) mod 2^33`.
    #[must_use]
    pub fn splice_pts(&self) -> Option<u64> {
        self.pts_time().map(|t| pts_add(t, self.pts_adjustment))
    }

    /// Whether this command takes effect at the carrying packet.
    #[must_use]
    pub const fn is_immediate(&self) -> bool {
        match &self.splice_command {
            Command::SpliceInsert(insert) => matches!(insert.splice_immediate, Some(true)),
            Command::TimeSignal { pts_time } => pts_time.is_none(),
            _ => false,
        }
    }

    /// Declared duration in ticks, preferring `break_duration`.
    #[must_use]
    pub fn duration_ticks(&self) -> Option<u64> {
        if let Command::SpliceInsert(SpliceInsertCommand {
            break_duration: Some(duration),
            ..
        }) = &self.splice_command
        {
            return Some(duration.duration);
        }
        self.primary_segmentation()
            .and_then(|s| s.segmentation_duration)
    }

    /// A short operator-facing name.
    #[must_use]
    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .primary_segmentation()
            .and_then(|s| s.segmentation_type_name.clone())
        {
            return name;
        }
        if let Command::SpliceInsert(insert) = &self.splice_command {
            return match (insert.splice_event_cancel, insert.out_of_network) {
                (true, _) => "Splice Insert (Cancel)",
                (false, Some(false)) => "Splice Insert (Return)",
                (false, _) => "Splice Insert (Out of Network)",
            }
            .to_owned();
        }
        self.splice_command_name.to_owned()
    }
}

/// `splice_command_type` table name.
#[must_use]
pub const fn command_name(command_type: u8) -> &'static str {
    match command_type {
        0x00 => "splice_null",
        0x04 => "splice_schedule",
        0x05 => "splice_insert",
        0x06 => "time_signal",
        0x07 => "bandwidth_reservation",
        0xFF => "private_command",
        _ => "reserved",
    }
}

/// `segmentation_type_id` table name, including the identification, runover and
/// override rows the marker labels do not use.
#[must_use]
pub fn segmentation_name(type_id: u8) -> Option<&'static str> {
    Some(match type_id {
        0x00 => "Not Indicated",
        0x01 => "Content Identification",
        0x15 => "Program Runover Planned",
        0x16 => "Program Runover Unplanned",
        0x18 => "Program Blackout Override",
        0x19 => "Program Start - In Progress",
        other => return super::segmentation_type_name(other),
    })
}

/// `segmentation_upid_type` table name.
#[must_use]
pub const fn upid_type_name(upid_type: u8) -> &'static str {
    match upid_type {
        0x00 => "Not Used",
        0x01 => "User Defined",
        0x02 => "ISCI",
        0x03 => "Ad-ID",
        0x04 => "UMID",
        0x05 => "ISAN (deprecated)",
        0x06 => "ISAN",
        0x07 => "TID",
        0x08 => "AiringID",
        0x09 => "ADI",
        0x0A => "EIDR",
        0x0B => "ATSC Content Identifier",
        0x0C => "MPU",
        0x0D => "MID",
        0x0E => "ADS Information",
        0x0F => "URI",
        0x10 => "UUID",
        0x11 => "SCR",
        _ => "Reserved",
    }
}

fn grouped_hex(bytes: &[u8], group: usize, separator: &str) -> String {
    bytes
        .chunks(group)
        .map(hex)
        .collect::<Vec<_>>()
        .join(separator)
        .to_uppercase()
}

/// ISO 7064 Mod 37,36 check character over the 20 hex digits of an EIDR suffix.
fn eidr_check(suffix: &str) -> Option<char> {
    let mut product = 36_u32;
    for digit in suffix.chars() {
        let sum = (product + digit.to_digit(36)?) % 36;
        let sum = if sum == 0 { 36 } else { sum };
        product = (sum * 2) % 37;
    }
    char::from_digit((37 - product) % 36, 36).map(|c| c.to_ascii_uppercase())
}

fn eidr(bytes: &[u8]) -> Option<String> {
    let (prefix, suffix) = bytes.split_at_checked(2)?;
    let [high, low] = *prefix else { return None };
    let prefix = u16::from_be_bytes([high, low]);
    let digits = hex(suffix).to_uppercase();
    let check = eidr_check(&digits)?;
    Some(format!(
        "10.{prefix}/{}-{check}",
        grouped_hex(suffix, 2, "-")
    ))
}

fn uuid(bytes: &[u8]) -> Option<String> {
    let text = hex(bytes);
    Some(format!(
        "{}-{}-{}-{}-{}",
        text.get(0..8)?,
        text.get(8..12)?,
        text.get(12..16)?,
        text.get(16..20)?,
        text.get(20..32)?
    ))
}

fn atsc(bytes: &[u8]) -> Option<String> {
    let mut bits = Bits::new(bytes);
    let tsid = bits.u16(16)?;
    bits.read(2)?;
    let end_of_day = bits.u8(5)?;
    let unique_for = bits.u16(9)?;
    Some(format!(
        "tsid={tsid} end_of_day={end_of_day} unique_for={unique_for} content_id={}",
        text_or_hex(bits.rest())
    ))
}

fn mpu(bytes: &[u8]) -> Option<String> {
    let (format, private) = bytes.split_at_checked(4)?;
    Some(format!(
        "format_identifier={} private_data=0x{}",
        text_or_hex(format),
        hex(private)
    ))
}

fn upid_value(upid_type: u8, bytes: &[u8]) -> Option<String> {
    let exact = |len: usize| bytes.len() == len;
    match upid_type {
        0x00 if bytes.is_empty() => Some(String::new()),
        0x02 | 0x03 | 0x07 | 0x09 | 0x0E | 0x0F | 0x11 => printable(bytes),
        0x04 if exact(32) => Some(grouped_hex(bytes, 4, ".")),
        0x05 if exact(8) => Some(grouped_hex(bytes, 2, "-")),
        0x06 if exact(12) => Some(grouped_hex(bytes, 2, "-")),
        0x08 if exact(8) => Some(format!("0x{}", hex(bytes).to_uppercase())),
        0x0A if exact(12) => eidr(bytes),
        0x0B => atsc(bytes),
        0x0C => mpu(bytes),
        0x10 if exact(16) => uuid(bytes),
        _ => None,
    }
}

fn decode_upid(upid_type: u8, bytes: &[u8]) -> Upid {
    let nested = if upid_type == 0x0D {
        decode_mid(bytes)
    } else {
        Vec::new()
    };
    let value = if upid_type == 0x0D {
        nested
            .iter()
            .map(|u| format!("{}:{}", u.upid_type_name, u.value))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        upid_value(upid_type, bytes).unwrap_or_else(|| text_or_hex(bytes))
    };
    Upid {
        upid_type,
        upid_type_name: upid_type_name(upid_type),
        value,
        raw: hex(bytes),
        nested,
    }
}

fn decode_mid(bytes: &[u8]) -> Vec<Upid> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while let [upid_type, length, tail @ ..] = rest {
        let Some((value, next)) = tail.split_at_checked(usize::from(*length)) else {
            break;
        };
        // A MID inside a MID is not valid; keep it opaque rather than recurse.
        out.push(if *upid_type == 0x0D {
            Upid {
                upid_type: *upid_type,
                upid_type_name: upid_type_name(*upid_type),
                value: text_or_hex(value),
                raw: hex(value),
                nested: Vec::new(),
            }
        } else {
            decode_upid(*upid_type, value)
        });
        rest = next;
    }
    out
}

const fn device_restrictions_name(value: u8) -> &'static str {
    match value {
        0 => "Restrict Group 0",
        1 => "Restrict Group 1",
        2 => "Restrict Group 2",
        _ => "None",
    }
}

fn parse_segmentation(body: &[u8]) -> Option<Segmentation> {
    let mut bits = Bits::new(body);
    let segmentation_event_id = bits.u32(32)?;
    let segmentation_event_cancel = bits.flag()?;
    let segmentation_event_id_compliance_indicator = bits.flag()?;
    bits.read(6)?;
    let mut out = Segmentation {
        segmentation_event_id,
        segmentation_event_cancel,
        segmentation_event_id_compliance_indicator,
        program_segmentation: None,
        delivery_not_restricted: None,
        delivery_restrictions: None,
        components: Vec::new(),
        segmentation_duration: None,
        segmentation_duration_ms: None,
        upid: None,
        segmentation_type_id: None,
        segmentation_type_name: None,
        segment_num: None,
        segments_expected: None,
        sub_segment_num: None,
        sub_segments_expected: None,
    };
    if segmentation_event_cancel {
        return Some(out);
    }
    let program = bits.flag()?;
    let has_duration = bits.flag()?;
    let not_restricted = bits.flag()?;
    out.program_segmentation = Some(program);
    out.delivery_not_restricted = Some(not_restricted);
    if not_restricted {
        bits.read(5)?;
    } else {
        let web_delivery_allowed = bits.flag()?;
        let no_regional_blackout = bits.flag()?;
        let archive_allowed = bits.flag()?;
        let device_restrictions = bits.u8(2)?;
        out.delivery_restrictions = Some(DeliveryRestrictions {
            web_delivery_allowed,
            no_regional_blackout,
            archive_allowed,
            device_restrictions,
            device_restrictions_name: device_restrictions_name(device_restrictions),
        });
    }
    if !program {
        for _ in 0..bits.u8(8)? {
            let component_tag = bits.u8(8)?;
            bits.read(7)?;
            let pts_offset = bits.read(33)?;
            out.components.push(SegmentationComponent {
                component_tag,
                pts_offset,
            });
        }
    }
    if has_duration {
        let ticks = bits.read(40)?;
        out.segmentation_duration = Some(ticks);
        out.segmentation_duration_ms = Some(ticks_to_ms(ticks));
    }
    let upid_type = bits.u8(8)?;
    let upid_length = usize::from(bits.u8(8)?);
    out.upid = Some(decode_upid(upid_type, bits.take(upid_length)?));
    let type_id = bits.u8(8)?;
    out.segmentation_type_id = Some(type_id);
    out.segmentation_type_name = Some(
        segmentation_name(type_id).map_or_else(|| format!("Type 0x{type_id:02X}"), str::to_owned),
    );
    out.segment_num = Some(bits.u8(8)?);
    out.segments_expected = Some(bits.u8(8)?);
    // Sub-segment fields exist only for these types, and are optional there
    // because older encoders omit them.
    if matches!(type_id, 0x30 | 0x32 | 0x34 | 0x36 | 0x38 | 0x3A | 0x44 | 0x46) {
        out.sub_segment_num = bits.u8(8);
        out.sub_segments_expected = bits.u8(8);
    }
    Some(out)
}

fn parse_audio(body: &[u8]) -> Option<Vec<AudioComponent>> {
    let mut bits = Bits::new(body);
    let count = bits.u8(4)?;
    bits.read(4)?;
    let mut components = Vec::new();
    for _ in 0..count {
        let component_tag = bits.u8(8)?;
        let iso_code = text_or_hex(bits.take(3)?);
        let bit_stream_mode = bits.u8(3)?;
        let num_channels = bits.u8(4)?;
        let full_srvc_audio = bits.flag()?;
        components.push(AudioComponent {
            component_tag,
            iso_code,
            bit_stream_mode,
            num_channels,
            full_srvc_audio,
        });
    }
    Some(components)
}

fn parse_descriptor_body(tag: u8, body: &[u8]) -> Option<DescriptorBody> {
    let mut bits = Bits::new(body);
    Some(match tag {
        0x00 => DescriptorBody::Avail {
            provider_avail_id: bits.u32(32)?,
        },
        0x01 => {
            let preroll = bits.u8(8)?;
            let count = usize::from(bits.u8(3)?);
            bits.read(5)?;
            DescriptorBody::Dtmf {
                preroll,
                dtmf: text_or_hex(bits.take(count)?),
            }
        }
        0x02 => DescriptorBody::Segmentation(Box::new(parse_segmentation(body)?)),
        0x03 => DescriptorBody::Time {
            tai_seconds: bits.read(48)?,
            tai_ns: bits.u32(32)?,
            utc_offset: bits.u16(16)?,
        },
        0x04 => DescriptorBody::Audio {
            components: parse_audio(body)?,
        },
        _ => return None,
    })
}

fn parse_descriptors(mut loop_bytes: &[u8], parse_error: &mut Option<String>) -> Vec<Descriptor> {
    let mut out = Vec::new();
    while !loop_bytes.is_empty() {
        let [tag, length, tail @ ..] = loop_bytes else {
            *parse_error = Some("truncated splice descriptor header".to_owned());
            break;
        };
        let Some((content, next)) = tail.split_at_checked(usize::from(*length)) else {
            *parse_error = Some(format!("splice descriptor 0x{tag:02X} overruns its loop"));
            break;
        };
        let Some((identifier, body)) = content.split_at_checked(4) else {
            *parse_error = Some(format!("splice descriptor 0x{tag:02X} has no identifier"));
            loop_bytes = next;
            continue;
        };
        let cuei = identifier == b"CUEI";
        let parsed = cuei.then(|| parse_descriptor_body(*tag, body)).flatten();
        if cuei && parsed.is_none() && *tag <= 0x04 {
            *parse_error = Some(format!("splice descriptor 0x{tag:02X} is truncated"));
        }
        out.push(Descriptor {
            tag: *tag,
            identifier: text_or_hex(identifier),
            body: parsed.unwrap_or_else(|| DescriptorBody::Opaque { raw: hex(body) }),
        });
        loop_bytes = next;
    }
    out
}

fn parse_splice_time(bits: &mut Bits<'_>) -> Option<Option<u64>> {
    if bits.flag()? {
        bits.read(6)?;
        Some(Some(bits.read(33)?))
    } else {
        bits.read(7)?;
        Some(None)
    }
}

fn parse_break_duration(bits: &mut Bits<'_>) -> Option<BreakDuration> {
    let auto_return = bits.flag()?;
    bits.read(6)?;
    let duration = bits.read(33)?;
    Some(BreakDuration {
        auto_return,
        duration,
        duration_ms: ticks_to_ms(duration),
    })
}

fn parse_insert(bits: &mut Bits<'_>) -> Option<SpliceInsertCommand> {
    let splice_event_id = bits.u32(32)?;
    let splice_event_cancel = bits.flag()?;
    bits.read(7)?;
    let mut out = SpliceInsertCommand {
        splice_event_id,
        splice_event_cancel,
        out_of_network: None,
        program_splice: None,
        splice_immediate: None,
        event_id_compliance: None,
        pts_time: None,
        components: Vec::new(),
        break_duration: None,
        unique_program_id: None,
        avail_num: None,
        avails_expected: None,
    };
    if splice_event_cancel {
        return Some(out);
    }
    out.out_of_network = Some(bits.flag()?);
    let program = bits.flag()?;
    let has_duration = bits.flag()?;
    let immediate = bits.flag()?;
    out.program_splice = Some(program);
    out.splice_immediate = Some(immediate);
    out.event_id_compliance = Some(bits.flag()?);
    bits.read(3)?;
    if program && !immediate {
        out.pts_time = parse_splice_time(bits)?;
    }
    if !program {
        for _ in 0..bits.u8(8)? {
            let component_tag = bits.u8(8)?;
            let pts_time = if immediate {
                None
            } else {
                parse_splice_time(bits)?
            };
            out.components.push(SpliceComponent {
                component_tag,
                pts_time,
            });
        }
    }
    if has_duration {
        out.break_duration = Some(parse_break_duration(bits)?);
    }
    out.unique_program_id = Some(bits.u16(16)?);
    out.avail_num = Some(bits.u8(8)?);
    out.avails_expected = Some(bits.u8(8)?);
    Some(out)
}

fn parse_schedule(bits: &mut Bits<'_>) -> Option<Vec<ScheduledSplice>> {
    let mut events = Vec::new();
    for _ in 0..bits.u8(8)? {
        let splice_event_id = bits.u32(32)?;
        let splice_event_cancel = bits.flag()?;
        bits.read(7)?;
        let mut event = ScheduledSplice {
            splice_event_id,
            splice_event_cancel,
            out_of_network: None,
            utc_splice_time: None,
            utc_splice_time_unix: None,
            break_duration: None,
            unique_program_id: None,
            avail_num: None,
            avails_expected: None,
        };
        if !splice_event_cancel {
            event.out_of_network = Some(bits.flag()?);
            let program = bits.flag()?;
            let has_duration = bits.flag()?;
            bits.read(5)?;
            if program {
                let time = bits.u32(32)?;
                event.utc_splice_time = Some(time);
                event.utc_splice_time_unix = Some(u64::from(time) + GPS_EPOCH_UNIX_SECONDS);
            } else {
                for _ in 0..bits.u8(8)? {
                    bits.read(8)?;
                    bits.read(32)?;
                }
            }
            if has_duration {
                event.break_duration = Some(parse_break_duration(bits)?);
            }
            event.unique_program_id = Some(bits.u16(16)?);
            event.avail_num = Some(bits.u8(8)?);
            event.avails_expected = Some(bits.u8(8)?);
        }
        events.push(event);
    }
    Some(events)
}

fn parse_command(command_type: u8, bytes: &[u8]) -> Option<(Command, usize)> {
    let mut bits = Bits::new(bytes);
    let command = match command_type {
        0x00 => Command::SpliceNull,
        0x04 => Command::SpliceSchedule {
            events: parse_schedule(&mut bits)?,
        },
        0x05 => Command::SpliceInsert(parse_insert(&mut bits)?),
        0x06 => Command::TimeSignal {
            pts_time: parse_splice_time(&mut bits)?,
        },
        0x07 => Command::BandwidthReservation,
        0xFF => Command::PrivateCommand {
            identifier: hex(bits.take(4)?),
            private_bytes: hex(bits.rest()),
        },
        _ => Command::Opaque { raw: hex(bytes) },
    };
    Some((command, bits.pos.div_ceil(8)))
}

/// Decode one section as far as its bytes allow.
///
/// A CRC failure or truncated tail is reported in the returned value next to
/// whatever was read. An encrypted section stops at the clear header.
///
/// # Errors
/// A description of the problem when the fixed header itself cannot be read.
pub fn decode_section(section: &[u8]) -> Result<DecodedSection, String> {
    let mut bits = Bits::new(section);
    let header = || "truncated splice_info_section header".to_owned();
    let table_id = bits.u8(8).ok_or_else(header)?;
    if table_id != 0xFC {
        return Err(format!("table_id 0x{table_id:02X} is not 0xFC"));
    }
    bits.read(2).ok_or_else(header)?;
    let sap_type = bits.u8(2).ok_or_else(header)?;
    let section_length = bits.u16(12).ok_or_else(header)?;
    let protocol_version = bits.u8(8).ok_or_else(header)?;
    let encrypted_packet = bits.flag().ok_or_else(header)?;
    let encryption_algorithm = bits.u8(6).ok_or_else(header)?;
    let pts_adjustment = bits.read(33).ok_or_else(header)?;
    let cw_index = bits.u8(8).ok_or_else(header)?;
    let tier = bits.u16(12).ok_or_else(header)?;
    let splice_command_length = bits.u16(12).ok_or_else(header)?;
    let splice_command_type = bits.u8(8).ok_or_else(header)?;
    let body_start = bits.pos / 8;

    let declared_end = usize::from(section_length).saturating_add(3);
    let mut parse_error = (declared_end > section.len()).then(|| {
        format!(
            "section_length declares {declared_end} bytes, {} present",
            section.len()
        )
    });
    let end = declared_end.min(section.len());
    let crc_start = end.saturating_sub(4).max(body_start);
    let crc_bytes = section.get(crc_start..end).unwrap_or_default();
    let carried: Option<[u8; 4]> = crc_bytes.try_into().ok();
    let crc_ok = carried.is_some_and(|carried| {
        section
            .get(..crc_start)
            .is_some_and(|body| crate::mpegts::crc::crc32_mpeg2(body) == u32::from_be_bytes(carried))
    });
    let crc_32 = format!("0x{}", hex(crc_bytes));
    let payload = section.get(body_start..crc_start).unwrap_or_default();

    let mut descriptors = Vec::new();
    let splice_command = if encrypted_packet {
        Command::Opaque { raw: hex(payload) }
    } else {
        let declared = (splice_command_length != 0xFFF).then_some(usize::from(splice_command_length));
        let command_bytes = declared.map_or(payload, |len| {
            payload.get(..len).unwrap_or(payload)
        });
        match parse_command(splice_command_type, command_bytes) {
            Some((command, used)) => {
                let after = declared.unwrap_or(used);
                if let Some([high, low, tail @ ..]) = payload.get(after..) {
                    let loop_length = usize::from(u16::from_be_bytes([*high, *low]));
                    match tail.get(..loop_length) {
                        Some(loop_bytes) => {
                            descriptors = parse_descriptors(loop_bytes, &mut parse_error);
                        }
                        None => {
                            parse_error =
                                Some("descriptor_loop_length overruns the section".to_owned());
                            descriptors = parse_descriptors(tail, &mut parse_error);
                        }
                    }
                }
                command
            }
            None => {
                parse_error = Some(format!(
                    "{} command is truncated",
                    command_name(splice_command_type)
                ));
                Command::Opaque {
                    raw: hex(command_bytes),
                }
            }
        }
    };
    Ok(DecodedSection {
        table_id,
        sap_type,
        section_length,
        protocol_version,
        encrypted_packet,
        encryption_algorithm,
        pts_adjustment,
        cw_index,
        tier,
        splice_command_length,
        splice_command_type,
        splice_command_name: command_name(splice_command_type),
        splice_command,
        descriptors,
        crc_32,
        crc_ok,
        parse_error,
    })
}
