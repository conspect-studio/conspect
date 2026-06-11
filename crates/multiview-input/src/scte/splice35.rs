//! SCTE-35 `splice_info_section` parser (ANSI/SCTE 35), pure.
//!
//! A `splice_info_section` is a private MPEG-2 section (`table_id` `0xFC`) whose
//! body carries one splice command. The two commands Multiview acts on are:
//!
//! * `splice_insert` (command type `0x05`) — the classic in/out cue with a
//!   `splice_event_id`, an out-of-network flag (out = start of break), an
//!   optional `splice_time` (33-bit 90 kHz PTS, or *immediate*), and an optional
//!   `break_duration`.
//! * `time_signal` (command type `0x06`) — a bare `splice_time` whose meaning is
//!   carried by the trailing segmentation descriptors.
//!
//! The section ends with a CRC-32/MPEG-2 (the same polynomial as PSI). This
//! parser validates that CRC, decodes the command, and projects a normalised
//! [`crate::scte::CueEvent`].
//!
//! All field extraction uses a panic-free internal bit reader; no `unsafe`, bounded
//! allocation, and malformed input surfaces as a typed [`crate::scte::ScteError`].

use crate::scte::{CueEvent, CueKind, ScteError};

/// The `table_id` of a SCTE-35 `splice_info_section`.
pub const TABLE_ID: u8 = 0xFC;

/// The `splice_null` command type (`0x00`).
pub const CMD_SPLICE_NULL: u8 = 0x00;

/// The `splice_insert` command type (`0x05`).
pub const CMD_SPLICE_INSERT: u8 = 0x05;

/// The `time_signal` command type (`0x06`).
pub const CMD_TIME_SIGNAL: u8 = 0x06;

/// A panic-free MSB-first bit reader over a byte slice.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    /// Read `width` bits (`0..=64`) MSB-first as a `u64`.
    fn read(&mut self, width: usize) -> Result<u64, ScteError> {
        if width > 64 {
            return Err(ScteError::Syntax("bit read width exceeds 64"));
        }
        let mut value: u64 = 0;
        for _ in 0..width {
            let byte_index = self.bit_pos / 8;
            let bit_in_byte = 7 - (self.bit_pos % 8);
            let byte = *self.bytes.get(byte_index).ok_or(ScteError::TooShort {
                need: byte_index.saturating_add(1),
                got: self.bytes.len(),
            })?;
            let bit = (byte >> bit_in_byte) & 1;
            value = (value << 1) | u64::from(bit);
            self.bit_pos = self.bit_pos.checked_add(1).ok_or(ScteError::Overrun {
                declared: self.bit_pos,
                available: self.bytes.len().saturating_mul(8),
            })?;
        }
        Ok(value)
    }

    /// Read a single flag bit.
    fn flag(&mut self) -> Result<bool, ScteError> {
        Ok(self.read(1)? != 0)
    }

    /// Skip `width` bits.
    fn skip(&mut self, width: usize) -> Result<(), ScteError> {
        let _ = self.read(width)?;
        Ok(())
    }
}

/// A decoded `splice_insert` command.
// reason: each flag mirrors a distinct SCTE-35 `splice_insert` wire bit
// (`splice_event_cancel_indicator`, `out_of_network_indicator`,
// `splice_immediate_flag`, `auto_return`); collapsing them into an enum/bitset
// would obscure the 1:1 mapping to the standard and hurt readability.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceInsert {
    /// The unique splice event id.
    pub splice_event_id: u32,
    /// Whether this command cancels a previously-signalled event.
    pub cancel: bool,
    /// Whether the splice is *out of network* (the start of an ad break).
    pub out_of_network: bool,
    /// Whether the splice is to be applied immediately (no `splice_time`).
    pub immediate: bool,
    /// The 33-bit 90 kHz splice PTS, when not immediate.
    pub pts_time_90k: Option<u64>,
    /// The break duration in 90 kHz ticks, when present.
    pub break_duration_90k: Option<u64>,
    /// Whether the break auto-returns to the network feed at duration end.
    pub auto_return: bool,
}

/// A decoded `time_signal` command (a bare `splice_time`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignal {
    /// The 33-bit 90 kHz PTS, when specified.
    pub pts_time_90k: Option<u64>,
}

/// The splice command carried by a section (only the commands Multiview acts on are
/// fully decoded; others are recorded by type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpliceCommand {
    /// `splice_null` — a heartbeat with no action.
    Null,
    /// `splice_insert` — the classic in/out cue.
    Insert(SpliceInsert),
    /// `time_signal` — a bare timing signal.
    TimeSignal(TimeSignal),
    /// A recognised-but-not-decoded command, preserving its type byte.
    Other(u8),
}

/// A parsed SCTE-35 `splice_info_section`.
///
/// The exact section bytes the parser validated are retained so the section can
/// be re-emitted byte-for-byte with only its `pts_adjustment` rewritten — see
/// [`SpliceInfoSection::reserialize_with_pts_adjustment`]. Retaining the bytes
/// makes the type `Clone` but not `Copy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpliceInfoSection {
    /// The `pts_adjustment` (33-bit) added to all PTS in the section.
    pub pts_adjustment: u64,
    /// The `splice_command_type`.
    pub command_type: u8,
    /// The decoded command.
    pub command: SpliceCommand,
    /// The exact validated section bytes (header + body + trailing CRC), retained
    /// so the section can be re-serialised with a rewritten `pts_adjustment` and a
    /// fresh CRC without re-encoding every other field byte-by-byte.
    raw: Box<[u8]>,
    /// Whether the carried command body was flagged `encrypted_packet`. An
    /// encrypted section's command fields were not decoded, so re-serialise
    /// refuses it rather than emit a section it cannot vouch for.
    encrypted: bool,
}

impl SpliceInfoSection {
    /// Parse a SCTE-35 `splice_info_section` from a complete section (header +
    /// body + CRC).
    ///
    /// # Errors
    ///
    /// * [`ScteError::TooShort`] when the buffer cannot hold the declared section.
    /// * [`ScteError::Syntax`] when the `table_id` / fixed bits are wrong.
    /// * [`ScteError::Crc`] when the trailing CRC does not validate.
    /// * [`ScteError::Overrun`] when a length field overruns the section.
    pub fn parse(section: &[u8]) -> Result<Self, ScteError> {
        // table_id(8) + ssi(1)+pi(1)+rsvd(2)+section_length(12) = 3-byte prefix.
        if section.len() < 3 {
            return Err(ScteError::TooShort {
                need: 3,
                got: section.len(),
            });
        }
        let table_id = *section.first().ok_or(ScteError::TooShort {
            need: 3,
            got: section.len(),
        })?;
        if table_id != TABLE_ID {
            return Err(ScteError::Syntax(
                "splice_info_section table_id is not 0xFC",
            ));
        }
        let b1 = *section.get(1).ok_or(ScteError::TooShort {
            need: 3,
            got: section.len(),
        })?;
        let b2 = *section.get(2).ok_or(ScteError::TooShort {
            need: 3,
            got: section.len(),
        })?;
        let section_length = (usize::from(b1 & 0b0000_1111) << 8) | usize::from(b2);
        let total = 3usize
            .checked_add(section_length)
            .ok_or(ScteError::Overrun {
                declared: section_length,
                available: section.len(),
            })?;
        let full = section.get(..total).ok_or(ScteError::TooShort {
            need: total,
            got: section.len(),
        })?;
        validate_crc(full)?;

        // The fields after section_length, per SCTE 35:
        //   protocol_version(8), encrypted_packet(1), encryption_algorithm(6),
        //   pts_adjustment(33), cw_index(8), tier(12),
        //   splice_command_length(12), splice_command_type(8), ...
        let after_length = full.get(3..).ok_or(ScteError::TooShort {
            need: 4,
            got: full.len(),
        })?;
        let mut reader = BitReader::new(after_length);
        let _protocol_version = reader.read(8)?;
        let encrypted = reader.flag()?;
        let _encryption_algorithm = reader.read(6)?;
        let pts_adjustment = reader.read(33)?;
        let _cw_index = reader.read(8)?;
        let _tier = reader.read(12)?;
        let splice_command_length = usize_from_u64(reader.read(12)?)?;
        let command_type = u8::try_from(reader.read(8)?)
            .map_err(|_e| ScteError::Syntax("splice_command_type out of range"))?;

        if encrypted {
            // Encrypted sections cannot be decoded without the control word; we
            // record the type but do not attempt to read the (ciphered) command.
            return Ok(Self {
                pts_adjustment,
                command_type,
                command: SpliceCommand::Other(command_type),
                raw: Box::from(full),
                encrypted: true,
            });
        }

        let command = match command_type {
            CMD_SPLICE_NULL => SpliceCommand::Null,
            CMD_SPLICE_INSERT => SpliceCommand::Insert(parse_splice_insert(&mut reader)?),
            CMD_TIME_SIGNAL => SpliceCommand::TimeSignal(parse_time_signal(&mut reader)?),
            other => SpliceCommand::Other(other),
        };
        // `splice_command_length` is informational here; the bit reader already
        // walked the exact command fields. (0xFFF means "unknown length".)
        let _ = splice_command_length;

        Ok(Self {
            pts_adjustment,
            command_type,
            command,
            raw: Box::from(full),
            encrypted: false,
        })
    }

    /// Project this section onto a normalised [`CueEvent`], if it carries one.
    ///
    /// Returns [`None`] for `splice_null` and for commands carrying no actionable
    /// cue. The `pts_adjustment` is folded into the reported presentation time.
    #[must_use]
    pub fn cue_event(&self) -> Option<CueEvent> {
        match self.command {
            SpliceCommand::Insert(insert) => {
                let kind = if insert.out_of_network {
                    CueKind::SpliceOut
                } else {
                    CueKind::SpliceIn
                };
                let pts_time_90k = insert
                    .pts_time_90k
                    .map(|t| t.wrapping_add(self.pts_adjustment) & MASK_33);
                Some(CueEvent {
                    kind,
                    event_id: insert.splice_event_id,
                    pts_time_90k,
                    break_duration_90k: insert.break_duration_90k,
                    cancel: insert.cancel,
                })
            }
            SpliceCommand::TimeSignal(ts) => Some(CueEvent {
                kind: CueKind::TimeSignal,
                event_id: 0,
                pts_time_90k: ts
                    .pts_time_90k
                    .map(|t| t.wrapping_add(self.pts_adjustment) & MASK_33),
                break_duration_90k: None,
                cancel: false,
            }),
            SpliceCommand::Null | SpliceCommand::Other(_) => None,
        }
    }

    /// Re-emit this `splice_info_section`'s bytes with the 33-bit `pts_adjustment`
    /// field replaced by `new_adjustment` (masked to 33 bits) and the trailing
    /// CRC-32/MPEG-2 recomputed over the rewritten section body.
    ///
    /// Every other field — `splice_command`, descriptors, `tier`, `cw_index`,
    /// each reserved bit — is byte-identical to the section the parser validated:
    /// only the five bytes that carry `pts_adjustment` (and the four CRC bytes)
    /// change. This is the SCTE-35 2023r1 re-stamp primitive: when an output
    /// splice rebases the program PTS timeline by a 90 kHz offset, the cue's
    /// `pts_adjustment` must shift by the **same** offset
    /// (see [`shift_pts_adjustment_90k`]) so downstream ad insertion stays
    /// aligned. An *immediate* splice (no `pts_time`) carries nothing in
    /// `pts_time` to shift, but its `pts_adjustment` field is still rewritten —
    /// pass `new_adjustment == self.pts_adjustment` to round-trip it verbatim.
    ///
    /// # Errors
    ///
    /// * [`ScteError::Unsupported`] when the section's command body is
    ///   `encrypted_packet` (its plaintext was never decoded, so the writer
    ///   cannot vouch for a faithful re-emit) — the original bytes are never
    ///   corrupted.
    /// * [`ScteError::TooShort`] when the retained section is too short to hold
    ///   the `pts_adjustment` field or the trailing CRC (a section the parser
    ///   would itself have rejected; defended in depth).
    pub fn reserialize_with_pts_adjustment(
        &self,
        new_adjustment: u64,
    ) -> Result<Vec<u8>, ScteError> {
        if self.encrypted {
            return Err(ScteError::Unsupported(u16::from(self.command_type)));
        }
        let adj = new_adjustment & MASK_33;
        let mut out = self.raw.to_vec();

        // The retained section must hold at least the bytes carrying
        // `pts_adjustment` (header bytes 0..=2, protocol_version byte 3, then the
        // five bytes [`ADJ_HI_BYTE`..=`ADJ_LO_LAST`] carrying
        // encrypted(1)/enc_alg(6)/pts_adjustment(33)) plus the four-byte trailing
        // CRC. `ADJ_LO_LAST + 1 + CRC_LEN == 13`, a small constant.
        if out.len() < ADJ_MIN_SECTION_LEN {
            return Err(ScteError::TooShort {
                need: ADJ_MIN_SECTION_LEN,
                got: out.len(),
            });
        }

        // The 33-bit field spans: bit 32 = low bit of `ADJ_HI_BYTE` (top 7 bits
        // hold encrypted + enc_alg, preserved); bits 31..0 = the four big-endian
        // bytes `ADJ_LO_FIRST..=ADJ_LO_LAST`.
        let bit32 = u8::try_from((adj >> 32) & 0x01)
            .map_err(|_e| ScteError::Syntax("pts_adjustment bit out of range"))?;
        let low32 = u32::try_from(adj & 0xFFFF_FFFF)
            .map_err(|_e| ScteError::Syntax("pts_adjustment low word out of range"))?;
        let low_bytes = low32.to_be_bytes();

        let hi = out.get_mut(ADJ_HI_BYTE).ok_or(ScteError::TooShort {
            need: ADJ_MIN_SECTION_LEN,
            got: 0,
        })?;
        *hi = (*hi & 0xFE) | bit32;

        let low_slot = out
            .get_mut(ADJ_LO_FIRST..=ADJ_LO_LAST)
            .ok_or(ScteError::TooShort {
                need: ADJ_MIN_SECTION_LEN,
                got: 0,
            })?;
        low_slot.copy_from_slice(&low_bytes);

        // Recompute the CRC-32/MPEG-2 over the rewritten body (everything except
        // the trailing four CRC bytes) and overwrite the trailing CRC.
        let body_len = out.len().saturating_sub(CRC_LEN);
        let body = out.get(..body_len).ok_or(ScteError::TooShort {
            need: CRC_LEN,
            got: out.len(),
        })?;
        let crc = crate::mpegts::crc::crc32_mpeg2(body);
        let crc_bytes = crc.to_be_bytes();
        let tail = out.get_mut(body_len..).ok_or(ScteError::TooShort {
            need: CRC_LEN,
            got: 0,
        })?;
        tail.copy_from_slice(&crc_bytes);

        Ok(out)
    }
}

/// 33-bit mask for PTS values.
const MASK_33: u64 = (1 << 33) - 1;

/// Byte index whose low bit carries `pts_adjustment` bit 32 (top 7 bits hold
/// `encrypted_packet` + `encryption_algorithm`). Header is bytes 0..=2 and
/// `protocol_version` is byte 3.
const ADJ_HI_BYTE: usize = 4;
/// First of the four big-endian bytes carrying `pts_adjustment` bits 31..0.
const ADJ_LO_FIRST: usize = 5;
/// Last of the four big-endian bytes carrying `pts_adjustment` bits 7..0.
const ADJ_LO_LAST: usize = 8;
/// Length of the trailing CRC-32 that closes every SCTE-35 section.
const CRC_LEN: usize = 4;
/// Smallest section length that can hold the `pts_adjustment` field and a CRC
/// (`ADJ_LO_LAST + 1` field bytes + `CRC_LEN`).
const ADJ_MIN_SECTION_LEN: usize = ADJ_LO_LAST + 1 + CRC_LEN;

/// Compute the seam-shifted `pts_adjustment` for an output splice: add the GP-6
/// 90 kHz timeline offset to the existing `pts_adjustment` and wrap at 2³³.
///
/// This is the SCTE-35 2023r1 re-stamp rule: a device that re-stamps PCR/PTS/DTS
/// and passes a cue message through should modify the `pts_adjustment` field by
/// the same amount the program timeline moved, so the cue's *effective* splice
/// time (`pts_time + pts_adjustment`) lands at the same program instant after the
/// rebase. Both arguments are reduced to 33 bits and the sum wraps mod 2³³,
/// matching the field's modular arithmetic. Pass `offset_90k == 0` to pass a cue
/// through unchanged (e.g. an immediate splice riding a zero-offset seam).
#[must_use]
pub fn shift_pts_adjustment_90k(old_adjustment: u64, offset_90k: u64) -> u64 {
    (old_adjustment.wrapping_add(offset_90k)) & MASK_33
}

/// Parse the `splice_insert` command body from the reader (positioned right after
/// `splice_command_type`).
fn parse_splice_insert(reader: &mut BitReader<'_>) -> Result<SpliceInsert, ScteError> {
    let splice_event_id = u32::try_from(reader.read(32)?)
        .map_err(|_e| ScteError::Syntax("splice_event_id out of range"))?;
    let cancel = reader.flag()?;
    reader.skip(7)?; // reserved
    if cancel {
        return Ok(SpliceInsert {
            splice_event_id,
            cancel: true,
            out_of_network: false,
            immediate: false,
            pts_time_90k: None,
            break_duration_90k: None,
            auto_return: false,
        });
    }
    let out_of_network = reader.flag()?;
    let program_splice = reader.flag()?;
    let duration_flag = reader.flag()?;
    let immediate = reader.flag()?;
    reader.skip(4)?; // reserved

    let mut pts_time_90k = None;
    if program_splice && !immediate {
        pts_time_90k = parse_splice_time(reader)?;
    }
    // Component splice mode (program_splice == false) carries a component count
    // and per-component splice_time; Multiview ingests program-level splices, so we
    // do not descend into components — the cue still carries its id/flags.
    if !program_splice && !immediate {
        let component_count = usize_from_u64(reader.read(8)?)?;
        for _ in 0..component_count {
            reader.skip(8)?; // component_tag
            let _ = parse_splice_time(reader)?;
        }
    }

    let mut break_duration_90k = None;
    let mut auto_return = false;
    if duration_flag {
        auto_return = reader.flag()?;
        reader.skip(6)?; // reserved
        break_duration_90k = Some(reader.read(33)?);
    }
    // unique_program_id(16), avail_num(8), avails_expected(8) follow; not needed.

    Ok(SpliceInsert {
        splice_event_id,
        cancel: false,
        out_of_network,
        immediate,
        pts_time_90k,
        break_duration_90k,
        auto_return,
    })
}

/// Parse the `time_signal` command body (a single `splice_time`).
fn parse_time_signal(reader: &mut BitReader<'_>) -> Result<TimeSignal, ScteError> {
    Ok(TimeSignal {
        pts_time_90k: parse_splice_time(reader)?,
    })
}

/// Parse a `splice_time()` structure: a `time_specified_flag`, then (when set) 6
/// reserved bits + a 33-bit PTS. Returns the PTS, or [`None`] when unspecified.
fn parse_splice_time(reader: &mut BitReader<'_>) -> Result<Option<u64>, ScteError> {
    let time_specified = reader.flag()?;
    if time_specified {
        reader.skip(6)?; // reserved
        Ok(Some(reader.read(33)?))
    } else {
        reader.skip(7)?; // reserved
        Ok(None)
    }
}

/// Validate the SCTE-35 section CRC-32/MPEG-2 (same algorithm as PSI).
fn validate_crc(section: &[u8]) -> Result<(), ScteError> {
    if section.len() < 4 {
        return Err(ScteError::TooShort {
            need: 4,
            got: section.len(),
        });
    }
    let body_len = section.len().saturating_sub(4);
    let body = section.get(..body_len).ok_or(ScteError::TooShort {
        need: 4,
        got: section.len(),
    })?;
    let crc_bytes = section.get(body_len..).ok_or(ScteError::TooShort {
        need: 4,
        got: section.len(),
    })?;
    let c0 = *crc_bytes
        .first()
        .ok_or(ScteError::TooShort { need: 4, got: 0 })?;
    let c1 = *crc_bytes
        .get(1)
        .ok_or(ScteError::TooShort { need: 4, got: 1 })?;
    let c2 = *crc_bytes
        .get(2)
        .ok_or(ScteError::TooShort { need: 4, got: 2 })?;
    let c3 = *crc_bytes
        .get(3)
        .ok_or(ScteError::TooShort { need: 4, got: 3 })?;
    let carried =
        (u32::from(c0) << 24) | (u32::from(c1) << 16) | (u32::from(c2) << 8) | u32::from(c3);
    let computed = crate::mpegts::crc::crc32_mpeg2(body);
    if carried == computed {
        Ok(())
    } else {
        Err(ScteError::Crc { carried, computed })
    }
}

/// Narrow a `u64` (read from a ≤16-bit field) to `usize`.
fn usize_from_u64(value: u64) -> Result<usize, ScteError> {
    usize::try_from(value).map_err(|_e| ScteError::Syntax("length field exceeds usize"))
}
