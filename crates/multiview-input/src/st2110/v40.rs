//! SMPTE **ST 2110-40** ancillary-data (ANC) RTP payload depacketizer (pure).
//!
//! ST 2110-40 (with IETF **RFC 8331** as the on-wire format) carries SMPTE ST
//! 291-1 ancillary data — timecode (RP 188 / ST 12), AFD (ST 2016-3), captions,
//! SCTE-104 — as a count of ANC data packets in the RTP payload:
//!
//! ```text
//!  0                   1                   2                   3
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |      Extended Sequence Number |            Length             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |ANC_Count      |F|         reserved ...                        |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |C| Line_Number |   Horizontal_Offset   |S| StreamNum |  DID    |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |   SDID  |Data_Count |     User_Data_Words ... (10-bit)        |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! Each ANC packet's header fields and User Data Words are **10-bit** symbols
//! packed MSB-first across byte boundaries, each ANC packet padded to a 32-bit
//! boundary. This module reads the high-level framing — the per-packet DID/SDID,
//! line/offset, and the decoded 8-bit user-data payload (10-bit words with their
//! parity/MSB stripped) — as a **pure**, allocation-bounded, panic-free parser.

/// Length of the ST 2110-40 payload header before the first ANC packet
/// (extended-sequence word + length word + `ANC_Count`/field word).
pub const PAYLOAD_HEADER_LEN: usize = 8;

/// The maximum number of ANC packets a single RTP payload may declare.
///
/// Bounds the per-packet `Vec` so a malicious `ANC_Count` cannot force unbounded
/// work; a real ST 2110-40 packet carries only a few ANC packets per frame.
pub const MAX_ANC_PACKETS: usize = 255;

/// Errors raised while depacketizing an ST 2110-40 RTP payload.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum V40Error {
    /// The payload was shorter than the header structure required.
    #[error("st2110-40 payload too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the parser required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },

    /// An ANC packet declared more user-data words than the payload can hold.
    #[error("st2110-40 anc packet runs past payload end")]
    Truncated,
}

/// One decoded ST 2110-40 ANC data packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AncPacket {
    /// The colour-channel flag (`C`): luma (`false`) or chroma (`true`).
    pub chroma: bool,
    /// The SDI line number this ANC packet was extracted from.
    pub line_number: u16,
    /// The horizontal offset within the line.
    pub horizontal_offset: u16,
    /// The Data Identifier (DID) — e.g. `0x41` for SMPTE ST 2016-3 AFD.
    pub did: u8,
    /// The Secondary Data Identifier (SDID) — e.g. `0x05` for AFD.
    pub sdid: u8,
    /// The decoded 8-bit User Data Words (10-bit symbols with parity stripped).
    pub user_data: Vec<u8>,
}

/// A depacketized ST 2110-40 payload: the reconstructed 32-bit sequence number
/// plus the ANC packets it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V40Payload {
    /// The full 32-bit sequence number: `(extended << 16) | rtp_sequence`.
    pub full_sequence: u32,
    /// The field flag (`F`) from the payload header.
    pub field: bool,
    /// The ANC packets carried (at most [`MAX_ANC_PACKETS`]).
    pub packets: Vec<AncPacket>,
}

/// A bit cursor that reads big-endian-packed symbols of arbitrary width
/// (MSB-first) from a byte slice without ever indexing out of range.
struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8], start_bit: usize) -> Self {
        Self {
            bytes,
            bit_pos: start_bit,
        }
    }

    /// Read `width` bits (`1..=16`) MSB-first as a `u16`. Returns [`None`] if the
    /// slice does not hold that many more bits.
    fn read(&mut self, width: usize) -> Option<u16> {
        if width == 0 || width > 16 {
            return None;
        }
        let mut value: u16 = 0;
        for _ in 0..width {
            let byte_index = self.bit_pos / 8;
            let bit_in_byte = 7 - (self.bit_pos % 8);
            let byte = *self.bytes.get(byte_index)?;
            let bit = (byte >> bit_in_byte) & 1;
            value = (value << 1) | u16::from(bit);
            self.bit_pos = self.bit_pos.checked_add(1)?;
        }
        Some(value)
    }

    /// Advance to the next 32-bit (word) boundary.
    fn align_to_word(&mut self) {
        let rem = self.bit_pos % 32;
        if rem != 0 {
            self.bit_pos = self.bit_pos.saturating_add(32 - rem);
        }
    }
}

impl V40Payload {
    /// Depacketize an ST 2110-40 RTP **payload** (bytes after the RTP fixed
    /// header), given the 16-bit `rtp_sequence` from the RTP header.
    ///
    /// Reads the payload header, then each ANC packet's 10-bit-packed header and
    /// user data, stripping the parity/MSB bit from each 10-bit symbol to yield
    /// 8-bit user-data bytes.
    ///
    /// # Errors
    ///
    /// * [`V40Error::TooShort`] if the payload cannot hold the header.
    /// * [`V40Error::Truncated`] if an ANC packet's declared word count runs past
    ///   the payload.
    pub fn parse(payload: &[u8], rtp_sequence: u16) -> Result<Self, V40Error> {
        if payload.len() < PAYLOAD_HEADER_LEN {
            return Err(V40Error::TooShort {
                need: PAYLOAD_HEADER_LEN,
                got: payload.len(),
            });
        }
        let ext = read_u16(payload, 0)?;
        let full_sequence = (u32::from(ext) << 16) | u32::from(rtp_sequence);
        // Per RFC 8331: bytes 0..2 = Extended Sequence Number, bytes 2..4 =
        // Length, byte 4 = ANC_Count, byte 5 top two bits = F (field), the rest of
        // bytes 5..8 reserved (the header is padded to a 32-bit word, so ANC data
        // starts at byte 8).
        let anc_count = usize::from(*payload.get(4).ok_or(V40Error::TooShort {
            need: 5,
            got: payload.len(),
        })?);
        let field = (*payload.get(5).ok_or(V40Error::TooShort {
            need: 6,
            got: payload.len(),
        })? & 0xC0)
            != 0;

        if anc_count > MAX_ANC_PACKETS {
            return Err(V40Error::Truncated);
        }

        let mut packets = Vec::with_capacity(anc_count.min(MAX_ANC_PACKETS));
        // ANC data starts at byte 8 (bit 64), 10-bit symbols, each ANC packet
        // padded to a 32-bit boundary.
        let mut reader = BitReader::new(payload, PAYLOAD_HEADER_LEN * 8);
        for _ in 0..anc_count {
            let packet = parse_anc(&mut reader)?;
            packets.push(packet);
            reader.align_to_word();
        }

        Ok(Self {
            full_sequence,
            field,
            packets,
        })
    }
}

/// Parse one ANC data packet from the bit reader (RFC 8331 ANC packet header +
/// 10-bit user-data words). Per RFC 8331 the on-wire ANC header preceding the
/// 10-bit symbols is: `C` (1 bit), `Line_Number` (11 bits), `Horizontal_Offset`
/// (12 bits), `S` (1 bit), `StreamNum` (7 bits), then the 10-bit DID, SDID and
/// `Data_Count` symbols, the user-data words, and a 10-bit checksum.
fn parse_anc(reader: &mut BitReader<'_>) -> Result<AncPacket, V40Error> {
    let chroma = reader.read(1).ok_or(V40Error::Truncated)? != 0;
    let line_number = reader.read(11).ok_or(V40Error::Truncated)?;
    let horizontal_offset = reader.read(12).ok_or(V40Error::Truncated)?;
    let _stream_flag = reader.read(1).ok_or(V40Error::Truncated)?;
    let _stream_num = reader.read(7).ok_or(V40Error::Truncated)?;

    let did = strip_parity(reader.read(10).ok_or(V40Error::Truncated)?);
    let sdid = strip_parity(reader.read(10).ok_or(V40Error::Truncated)?);
    let data_count = usize::from(strip_parity(reader.read(10).ok_or(V40Error::Truncated)?));

    let mut user_data = Vec::with_capacity(data_count);
    for _ in 0..data_count {
        let word = reader.read(10).ok_or(V40Error::Truncated)?;
        user_data.push(strip_parity(word));
    }
    // The 10-bit checksum word follows; we consume it but do not enforce it here
    // (the SDI checksum is over the ANC payload and is verified by higher-level
    // ANC handlers if required).
    let _checksum = reader.read(10).ok_or(V40Error::Truncated)?;

    Ok(AncPacket {
        chroma,
        line_number,
        horizontal_offset,
        did,
        sdid,
        user_data,
    })
}

/// Strip the parity/MSB bits from a 10-bit ANC symbol, returning the low 8 data
/// bits. (Bit 9 is even parity of bits 0..8; bit 8 is the inverse of bit 7.)
fn strip_parity(word: u16) -> u8 {
    u8::try_from(word & 0xFF).unwrap_or(0)
}

/// Read a big-endian `u16` at `offset`, or [`V40Error::TooShort`].
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, V40Error> {
    let end = offset.checked_add(2).ok_or(V40Error::TooShort {
        need: usize::MAX,
        got: bytes.len(),
    })?;
    let hi = *bytes.get(offset).ok_or(V40Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let lo = *bytes
        .get(offset.saturating_add(1))
        .ok_or(V40Error::TooShort {
            need: end,
            got: bytes.len(),
        })?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}

/// The bit offset where ANC data begins, for tests/diagnostics.
#[must_use]
pub const fn anc_data_start_bit() -> usize {
    PAYLOAD_HEADER_LEN * 8
}

/// Expose the reader's terminal position helper used in property tests: the bit
/// position is internal, so this re-exports the alignment helper as a free
/// function for symmetric test fixtures.
#[must_use]
pub const fn align_word(bit_pos: usize) -> usize {
    let rem = bit_pos % 32;
    if rem == 0 {
        bit_pos
    } else {
        bit_pos + (32 - rem)
    }
}
