//! SCTE-104 message parser (ANSI/SCTE 104), pure.
//!
//! SCTE-104 is the *operational* trigger an automation system sends to an
//! encoder/inserter; the encoder turns it into the SCTE-35 cue on the wire. It is
//! carried network-side in SMPTE **ST 2010** and into VANC via **ST 2031**.
//! Multiview parses it so that, on contribution feeds that carry SCTE-104 rather
//! than SCTE-35, the same [`crate::scte::CueEvent`] vocabulary is produced.
//!
//! This decodes the **`multiple_operation_message`** form (the one real plants
//! use), extracting `splice_request_data` operations (op id `0x0101`). Each
//! splice request carries a `splice_insert_type`, an event id, and a
//! pre-roll/break-duration pair. A single-operation message with the
//! splice op id is also handled.
//!
//! Pure, bounded, panic-free; malformed input surfaces as
//! [`crate::scte::ScteError`].

use crate::scte::{CueEvent, CueKind, ScteError};

/// The `multiple_operation_message` op id sentinel (`0xFFFF`) carried in the
/// `opID` field of a multi-op message header.
pub const MULTIPLE_OPERATION_OPID: u16 = 0xFFFF;

/// The `splice_request_data` op id (`0x0101`).
pub const OP_SPLICE_REQUEST: u16 = 0x0101;

/// The `time_signal_request_data` op id (`0x0104`).
pub const OP_TIME_SIGNAL: u16 = 0x0104;

/// SCTE-104 `splice_insert_type` values (SCTE 104 Table 8-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SpliceInsertType {
    /// reserved / not used (`0`).
    Reserved,
    /// `spliceStart_normal` (`1`) — scheduled start of a break.
    StartNormal,
    /// `spliceStart_immediate` (`2`) — immediate start of a break.
    StartImmediate,
    /// `spliceEnd_normal` (`3`) — scheduled end of a break.
    EndNormal,
    /// `spliceEnd_immediate` (`4`) — immediate end of a break.
    EndImmediate,
    /// `splice_cancel` (`5`).
    Cancel,
    /// Any other value, preserving the raw byte.
    Other(u8),
}

impl SpliceInsertType {
    /// Decode a `splice_insert_type` byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Reserved,
            1 => Self::StartNormal,
            2 => Self::StartImmediate,
            3 => Self::EndNormal,
            4 => Self::EndImmediate,
            5 => Self::Cancel,
            other => Self::Other(other),
        }
    }

    /// Whether this type begins a break (splice-out).
    #[must_use]
    pub const fn is_start(self) -> bool {
        matches!(self, Self::StartNormal | Self::StartImmediate)
    }

    /// Whether this type ends a break (splice-in).
    #[must_use]
    pub const fn is_end(self) -> bool {
        matches!(self, Self::EndNormal | Self::EndImmediate)
    }

    /// Whether this type is immediate (no pre-roll).
    #[must_use]
    pub const fn is_immediate(self) -> bool {
        matches!(self, Self::StartImmediate | Self::EndImmediate)
    }
}

/// One decoded `splice_request_data` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceRequest {
    /// The splice insert type.
    pub insert_type: SpliceInsertType,
    /// The 32-bit splice event id.
    pub event_id: u32,
    /// `unique_program_id`.
    pub unique_program_id: u16,
    /// Pre-roll time in milliseconds (lead time before the splice point).
    pub pre_roll_ms: u16,
    /// Break duration in tenths of a second (SCTE-104 unit).
    pub break_duration_tenths: u16,
    /// Whether the break auto-returns.
    pub auto_return: bool,
}

impl SpliceRequest {
    /// Project this request onto a normalised [`CueEvent`].
    ///
    /// The break duration (tenths of a second) and pre-roll convert to the 90 kHz
    /// program clock used by the cue vocabulary (`tenths * 9000`).
    #[must_use]
    pub fn cue_event(self) -> CueEvent {
        let kind = if self.insert_type.is_end() {
            CueKind::SpliceIn
        } else if self.insert_type.is_start() {
            CueKind::SpliceOut
        } else {
            CueKind::TimeSignal
        };
        let break_duration_90k = if self.break_duration_tenths == 0 {
            None
        } else {
            Some(u64::from(self.break_duration_tenths).saturating_mul(9000))
        };
        CueEvent {
            kind,
            event_id: self.event_id,
            // SCTE-104 carries pre-roll, not an absolute PTS; the encoder derives
            // the SCTE-35 PTS. We leave the cue's PTS unspecified (applies at the
            // signalled point + pre-roll downstream).
            pts_time_90k: None,
            break_duration_90k,
            cancel: matches!(self.insert_type, SpliceInsertType::Cancel),
        }
    }
}

/// A parsed SCTE-104 message: the splice requests it carried.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scte104Message {
    /// The splice requests (and time-signal requests) extracted, in order.
    pub requests: Vec<SpliceRequest>,
}

/// The fixed `multiple_operation_message` header length before the op loop:
/// `reserved`(2) + `messageSize`(2) + `protocol_version`(1) + `AS_index`(1) +
/// `message_number`(1) + `DPI_PID_index`(2) + `SCTE35_protocol_version`(1) +
/// timestamp + `num_ops`(1). The timestamp is variable (its type byte selects
/// length); we parse it explicitly below.
const MOM_FIXED_PREFIX: usize = 10;

/// Cap on the number of operations parsed from one message.
const MAX_OPS: usize = 256;

impl Scte104Message {
    /// Parse a SCTE-104 `multiple_operation_message`.
    ///
    /// Walks the header, the variable-length timestamp, then `numOps`
    /// operations, decoding `splice_request_data` / `time_signal_request_data`.
    ///
    /// # Errors
    ///
    /// * [`ScteError::TooShort`] when the buffer cannot hold the header / an op.
    /// * [`ScteError::Syntax`] when the message is not a multi-op message.
    /// * [`ScteError::Overrun`] when an op's declared length runs past the buffer.
    pub fn parse(bytes: &[u8]) -> Result<Self, ScteError> {
        if bytes.len() < MOM_FIXED_PREFIX {
            return Err(ScteError::TooShort {
                need: MOM_FIXED_PREFIX,
                got: bytes.len(),
            });
        }
        // reserved(16) must be 0xFFFF for a multiple_operation_message.
        let header_opid = read_u16(bytes, 0)?;
        if header_opid != MULTIPLE_OPERATION_OPID {
            return Err(ScteError::Syntax(
                "not a multiple_operation_message (reserved/opID != 0xFFFF)",
            ));
        }
        let _message_size = read_u16(bytes, 2)?;
        // protocol_version(1) @4, AS_index(1) @5, message_number(1) @6,
        // DPI_PID_index(2) @7..9, SCTE35_protocol_version(1) @9.
        // timestamp starts @10: time_type(1) selects the body length.
        let time_type = *bytes.get(10).ok_or(ScteError::TooShort {
            need: 11,
            got: bytes.len(),
        })?;
        let timestamp_len = match time_type {
            0 => 0, // none
            1 => 6, // UTC: seconds(4) + microseconds(2)
            2 => 5, // SMPTE VITC: hours/minutes/seconds/frames(4)+... → 5 bytes
            3 => 1, // GPI: GPI_number(1)
            _ => return Err(ScteError::Syntax("unknown SCTE-104 timestamp time_type")),
        };
        let num_ops_index = 11usize
            .checked_add(timestamp_len)
            .ok_or(ScteError::Overrun {
                declared: timestamp_len,
                available: bytes.len(),
            })?;
        let num_ops = usize::from(*bytes.get(num_ops_index).ok_or(ScteError::TooShort {
            need: num_ops_index.saturating_add(1),
            got: bytes.len(),
        })?);
        if num_ops > MAX_OPS {
            return Err(ScteError::Overrun {
                declared: num_ops,
                available: MAX_OPS,
            });
        }

        let mut offset = num_ops_index.checked_add(1).ok_or(ScteError::Overrun {
            declared: num_ops_index,
            available: bytes.len(),
        })?;
        let mut requests = Vec::new();
        for _ in 0..num_ops {
            // Each op: opID(2) + data_length(2) + data(data_length).
            let op_id = read_u16(bytes, offset)?;
            let data_length = usize::from(read_u16(bytes, offset.saturating_add(2))?);
            let data_start = offset.checked_add(4).ok_or(ScteError::Overrun {
                declared: offset,
                available: bytes.len(),
            })?;
            let data_end = data_start
                .checked_add(data_length)
                .ok_or(ScteError::Overrun {
                    declared: data_length,
                    available: bytes.len(),
                })?;
            let data = bytes.get(data_start..data_end).ok_or(ScteError::Overrun {
                declared: data_end,
                available: bytes.len(),
            })?;
            match op_id {
                OP_SPLICE_REQUEST => requests.push(parse_splice_request(data)?),
                OP_TIME_SIGNAL => requests.push(parse_time_signal_request(data)?),
                _ => {}
            }
            offset = data_end;
        }
        Ok(Self { requests })
    }

    /// The normalised cue events for every splice/time-signal request.
    #[must_use]
    pub fn cue_events(&self) -> Vec<CueEvent> {
        self.requests.iter().map(|r| r.cue_event()).collect()
    }
}

/// Parse a `splice_request_data` op body (SCTE 104 §8.3):
/// `splice_insert_type`(1), `splice_event_id`(4), `unique_program_id`(2),
/// `pre_roll_time`(2), `break_duration`(2), `avail_num`(1), `avails_expected`(1),
/// `auto_return_flag`(1).
fn parse_splice_request(data: &[u8]) -> Result<SpliceRequest, ScteError> {
    let insert_type = SpliceInsertType::from_byte(*data.first().ok_or(ScteError::TooShort {
        need: 14,
        got: data.len(),
    })?);
    let event_id = read_u32(data, 1)?;
    let unique_program_id = read_u16(data, 5)?;
    let pre_roll_ms = read_u16(data, 7)?;
    let break_duration_tenths = read_u16(data, 9)?;
    // avail_num @11, avails_expected @12, auto_return_flag @13.
    let auto_return = *data.get(13).ok_or(ScteError::TooShort {
        need: 14,
        got: data.len(),
    })? != 0;
    Ok(SpliceRequest {
        insert_type,
        event_id,
        unique_program_id,
        pre_roll_ms,
        break_duration_tenths,
        auto_return,
    })
}

/// Parse a `time_signal_request_data` op body: `pre_roll_time`(2). Modelled as a
/// time-signal cue with no in/out semantics.
fn parse_time_signal_request(data: &[u8]) -> Result<SpliceRequest, ScteError> {
    let pre_roll_ms = read_u16(data, 0)?;
    Ok(SpliceRequest {
        insert_type: SpliceInsertType::Reserved,
        event_id: 0,
        unique_program_id: 0,
        pre_roll_ms,
        break_duration_tenths: 0,
        auto_return: false,
    })
}

/// Read a big-endian `u16` at `offset`.
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ScteError> {
    let hi = *bytes.get(offset).ok_or(ScteError::TooShort {
        need: offset.saturating_add(2),
        got: bytes.len(),
    })?;
    let lo = *bytes
        .get(offset.saturating_add(1))
        .ok_or(ScteError::TooShort {
            need: offset.saturating_add(2),
            got: bytes.len(),
        })?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}

/// Read a big-endian `u32` at `offset`.
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ScteError> {
    let hi = read_u16(bytes, offset)?;
    let lo = read_u16(bytes, offset.saturating_add(2))?;
    Ok((u32::from(hi) << 16) | u32::from(lo))
}
