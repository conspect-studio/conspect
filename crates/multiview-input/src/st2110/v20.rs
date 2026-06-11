//! SMPTE **ST 2110-20** uncompressed-video RTP payload depacketizer (pure).
//!
//! ST 2110-20 carries raw video as pgroup-packed samples sliced into **Sample
//! Row Data (SRD)** segments. Each RTP payload begins with a 2-byte **Extended
//! Sequence Number** (the high 16 bits of the 32-bit sequence, the RTP header
//! carrying the low 16), then **one or more SRD headers**, then the packed
//! sample data the headers describe:
//!
//! ```text
//!  0                   1                   2                   3
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |    Extended Sequence Number   |            Length             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |F|          Line No            |C|           Offset            |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |          Length               |F|          Line No            | (more SRDs)
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! * **Extended Sequence Number** — high 16 bits of the 32-bit sequence number.
//! * Each **SRD header** is 6 bytes: a 16-bit `Length` (octets of sample data),
//!   a `Continuation` (`C`) bit + 15-bit `Line No` in the next word, and a
//!   `Field` (`F`) bit + 15-bit pixel `Offset` in the last word. When `C` = 1
//!   another SRD header follows; when `C` = 0 the sample data begins.
//!
//! This module is a **pure** byte-slice → typed value parser: it validates the
//! framing and lengths, never allocates beyond the small SRD-header vector, and
//! never panics on malformed input.

/// Length of the ST 2110-20 payload header's extended-sequence-number field.
pub const EXT_SEQ_LEN: usize = 2;

/// Length of one Sample Row Data (SRD) header, in bytes.
pub const SRD_HEADER_LEN: usize = 6;

/// Errors raised while depacketizing an ST 2110-20 RTP payload.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum V20Error {
    /// The payload was shorter than the header structure required.
    #[error("st2110-20 payload too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the parser required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },

    /// An SRD header declared more sample-data octets than the payload holds.
    #[error("st2110-20 srd declares {declared} octets, payload holds {available}")]
    Length {
        /// Octet count declared by the SRD `Length` field.
        declared: usize,
        /// Octets actually available for sample data.
        available: usize,
    },

    /// The payload claimed (via continuation bits) more SRD headers than a
    /// single RTP packet can sanely carry.
    #[error("st2110-20 too many srd segments (> {max})")]
    TooManySegments {
        /// The maximum number of SRD segments the parser will accept.
        max: usize,
    },
}

/// The maximum number of SRD segments a single RTP packet may declare.
///
/// A standard ST 2110-20 packet carries a small handful of row segments; this
/// bound caps the per-packet `Vec` allocation so a malicious continuation chain
/// cannot force unbounded work.
pub const MAX_SEGMENTS: usize = 64;

/// One decoded Sample Row Data segment: where its packed samples live within a
/// frame, and the byte range of the payload that holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SrdSegment {
    /// The picture line number these samples belong to (`0`-based).
    pub line_number: u16,
    /// The pixel offset of the first sample within the line.
    pub offset: u16,
    /// The `Field` bit: in interlaced transport, which field this row is in.
    pub field: bool,
    /// Byte offset within the *original payload* where this segment's packed
    /// sample data starts.
    pub data_start: usize,
    /// Length in bytes of this segment's packed sample data.
    pub data_len: usize,
}

impl SrdSegment {
    /// The byte range within the original payload holding this segment's data.
    #[must_use]
    pub const fn data_range(&self) -> core::ops::Range<usize> {
        self.data_start..(self.data_start + self.data_len)
    }
}

/// A depacketized ST 2110-20 video payload: the reconstructed 32-bit sequence
/// number plus the SRD segments it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V20Payload {
    /// The full 32-bit sequence number: `(extended << 16) | rtp_sequence`.
    pub full_sequence: u32,
    /// The SRD segments, in wire order (each pointing into the original
    /// payload).
    pub segments: Vec<SrdSegment>,
}

impl V20Payload {
    /// Depacketize an ST 2110-20 RTP **payload** (the bytes after the RTP fixed
    /// header), given the 16-bit `rtp_sequence` from the RTP header.
    ///
    /// Walks the extended-sequence word and the chain of SRD headers, validating
    /// that each declared sample-data length fits within the payload, and
    /// returns the segments with byte ranges pointing back into `payload`.
    ///
    /// # Errors
    ///
    /// * [`V20Error::TooShort`] if the payload cannot hold the ext-seq word, an
    ///   SRD header, or a segment's declared sample data.
    /// * [`V20Error::Length`] if an SRD `Length` exceeds the remaining payload.
    /// * [`V20Error::TooManySegments`] if the continuation chain exceeds
    ///   [`MAX_SEGMENTS`].
    pub fn parse(payload: &[u8], rtp_sequence: u16) -> Result<Self, V20Error> {
        let ext = read_u16(payload, 0)?;
        let full_sequence = (u32::from(ext) << 16) | u32::from(rtp_sequence);

        // First, read the SRD-header chain (each 6 bytes, continuation bit in the
        // line-number word). We accumulate (length, line, offset, field, cont)
        // tuples, then walk the sample data after the final header.
        let mut headers: Vec<(usize, u16, u16, bool)> = Vec::new();
        let mut offset = EXT_SEQ_LEN;
        loop {
            if headers.len() >= MAX_SEGMENTS {
                return Err(V20Error::TooManySegments { max: MAX_SEGMENTS });
            }
            let length = usize::from(read_u16(payload, offset)?);
            let line_word = read_u16(payload, offset.saturating_add(2))?;
            let off_word = read_u16(payload, offset.saturating_add(4))?;

            let continuation = (line_word & 0x8000) != 0;
            let line_number = line_word & 0x7FFF;
            let field = (off_word & 0x8000) != 0;
            let pixel_offset = off_word & 0x7FFF;

            headers.push((length, line_number, pixel_offset, field));
            offset = offset
                .checked_add(SRD_HEADER_LEN)
                .ok_or(V20Error::TooShort {
                    need: usize::MAX,
                    got: payload.len(),
                })?;
            if !continuation {
                break;
            }
        }

        // Now the sample data follows, one chunk per header, in order.
        let mut segments = Vec::with_capacity(headers.len());
        let mut data_pos = offset;
        for (length, line_number, pixel_offset, field) in headers {
            let end = data_pos.checked_add(length).ok_or(V20Error::Length {
                declared: length,
                available: payload.len().saturating_sub(data_pos),
            })?;
            if end > payload.len() {
                return Err(V20Error::Length {
                    declared: length,
                    available: payload.len().saturating_sub(data_pos),
                });
            }
            segments.push(SrdSegment {
                line_number,
                offset: pixel_offset,
                field,
                data_start: data_pos,
                data_len: length,
            });
            data_pos = end;
        }

        Ok(Self {
            full_sequence,
            segments,
        })
    }
}

/// Read a big-endian `u16` at `offset`, or [`V20Error::TooShort`].
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, V20Error> {
    let end = offset.checked_add(2).ok_or(V20Error::TooShort {
        need: usize::MAX,
        got: bytes.len(),
    })?;
    let hi = *bytes.get(offset).ok_or(V20Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let lo = *bytes
        .get(offset.saturating_add(1))
        .ok_or(V20Error::TooShort {
            need: end,
            got: bytes.len(),
        })?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}
