//! RTP fixed-header parser (IETF **RFC 3550** §5.1), the common substrate every
//! SMPTE ST 2110 essence rides over.
//!
//! ST 2110-20 (video), -30 (PCM audio) and -40 (ANC) all carry their payloads
//! in RTP. This module decodes the **fixed 12-byte RTP header** plus any CSRC
//! list and the (optional) one-word header-extension prefix, and hands back a
//! borrowed slice of the remaining payload. It is a **pure** byte-slice → typed
//! value codec: no sockets, no allocation beyond the (small, bounded) CSRC
//! vector, and it never panics on malformed input — a short or self-inconsistent
//! packet surfaces as a typed [`RtpError`].
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |V=2|P|X|  CC   |M|     PT      |       sequence number         |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                           timestamp                           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |           synchronization source (SSRC) identifier            |
//! +=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+=+
//! |            contributing source (CSRC) identifiers             |
//! |                             ....                              |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```

/// The RTP version this parser accepts (RFC 3550 fixes it at `2`).
pub const RTP_VERSION: u8 = 2;

/// The fixed RTP header length in bytes (before any CSRC list / extension).
pub const FIXED_HEADER_LEN: usize = 12;

/// Errors raised while parsing an RTP packet.
///
/// `#[non_exhaustive]`: downstream `match` arms must carry a wildcard so new
/// variants stay non-breaking. These convert into [`crate::Error`] via the
/// [`crate::st2110::St2110Error`] boundary type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RtpError {
    /// The buffer was shorter than the bytes the header (fixed + CSRC + ext)
    /// declares it must contain.
    #[error("rtp packet too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the header structure required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },

    /// The 2-bit version field was not [`RTP_VERSION`].
    #[error("rtp version {0} unsupported (only version {RTP_VERSION})")]
    BadVersion(u8),

    /// The trailing padding length byte (when the `P` bit is set) declared more
    /// padding than the payload holds.
    #[error("rtp padding length {declared} exceeds payload {available}")]
    BadPadding {
        /// Padding byte count declared by the final octet.
        declared: usize,
        /// Bytes available in the payload region.
        available: usize,
    },
}

/// A parsed RTP fixed header (RFC 3550 §5.1).
///
/// The `Copy` value carries every header field plus the byte offset at which the
/// payload begins; [`RtpPacket::parse`] additionally returns the payload slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// `M` marker bit. For ST 2110-20 it flags the **last packet of a frame**.
    pub marker: bool,
    /// 7-bit payload type (`PT`).
    pub payload_type: u8,
    /// 16-bit sequence number (wraps at 2^16; the hitless reconstructor and the
    /// jitter buffer unwrap it).
    pub sequence: u16,
    /// 32-bit media timestamp (units are essence-specific: 90 kHz video, sample
    /// rate for audio).
    pub timestamp: u32,
    /// 32-bit synchronization source identifier.
    pub ssrc: u32,
    /// Number of contributing-source identifiers (`CC`, `0..=15`).
    pub csrc_count: u8,
    /// Whether the header-extension (`X`) bit was set.
    pub has_extension: bool,
    /// The 16-bit extension profile id, present only when [`has_extension`] is
    /// set.
    ///
    /// [`has_extension`]: RtpHeader::has_extension
    pub extension_profile: Option<u16>,
}

/// A fully-parsed RTP packet: its [`RtpHeader`] plus a borrowed payload slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpPacket<'a> {
    /// The decoded fixed header.
    pub header: RtpHeader,
    /// The payload bytes (after the fixed header, CSRC list, and any extension;
    /// with trailing padding already removed).
    pub payload: &'a [u8],
}

impl<'a> RtpPacket<'a> {
    /// Parse an RTP packet from `bytes`.
    ///
    /// Validates the version, walks past the CSRC list and the (optional) header
    /// extension, and strips trailing padding when the `P` bit is set. The
    /// returned [`RtpPacket::payload`] borrows from `bytes`.
    ///
    /// # Errors
    ///
    /// * [`RtpError::TooShort`] if the buffer is smaller than the header
    ///   structure (fixed header + CSRC list + extension) declares.
    /// * [`RtpError::BadVersion`] if the version field is not [`RTP_VERSION`].
    /// * [`RtpError::BadPadding`] if the `P`-bit padding length is larger than
    ///   the remaining payload.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, RtpError> {
        if bytes.len() < FIXED_HEADER_LEN {
            return Err(RtpError::TooShort {
                need: FIXED_HEADER_LEN,
                got: bytes.len(),
            });
        }
        let b0 = *bytes.first().ok_or(RtpError::TooShort {
            need: FIXED_HEADER_LEN,
            got: bytes.len(),
        })?;
        let version = b0 >> 6;
        if version != RTP_VERSION {
            return Err(RtpError::BadVersion(version));
        }
        let has_padding = (b0 & 0b0010_0000) != 0;
        let has_extension = (b0 & 0b0001_0000) != 0;
        let csrc_count = b0 & 0b0000_1111;

        let b1 = *bytes.get(1).ok_or(RtpError::TooShort {
            need: FIXED_HEADER_LEN,
            got: bytes.len(),
        })?;
        let marker = (b1 & 0b1000_0000) != 0;
        let payload_type = b1 & 0b0111_1111;

        let sequence = read_u16(bytes, 2)?;
        let timestamp = read_u32(bytes, 4)?;
        let ssrc = read_u32(bytes, 8)?;

        // Skip the CSRC list: `csrc_count` 32-bit words after the fixed header.
        let csrc_bytes = usize::from(csrc_count)
            .checked_mul(4)
            .ok_or(RtpError::TooShort {
                need: usize::MAX,
                got: bytes.len(),
            })?;
        let mut offset = FIXED_HEADER_LEN
            .checked_add(csrc_bytes)
            .ok_or(RtpError::TooShort {
                need: usize::MAX,
                got: bytes.len(),
            })?;

        let mut extension_profile = None;
        if has_extension {
            // One extension header word: 16-bit profile + 16-bit length (in
            // 32-bit words of the following extension data).
            let profile = read_u16(bytes, offset)?;
            let ext_words = read_u16(bytes, offset.saturating_add(2))?;
            let ext_data = usize::from(ext_words)
                .checked_mul(4)
                .ok_or(RtpError::TooShort {
                    need: usize::MAX,
                    got: bytes.len(),
                })?;
            offset = offset
                .checked_add(4)
                .and_then(|o| o.checked_add(ext_data))
                .ok_or(RtpError::TooShort {
                    need: usize::MAX,
                    got: bytes.len(),
                })?;
            extension_profile = Some(profile);
        }

        let mut payload = bytes.get(offset..).ok_or(RtpError::TooShort {
            need: offset,
            got: bytes.len(),
        })?;

        if has_padding {
            let pad = *payload.last().ok_or(RtpError::BadPadding {
                declared: 1,
                available: 0,
            })?;
            let pad = usize::from(pad);
            if pad == 0 || pad > payload.len() {
                return Err(RtpError::BadPadding {
                    declared: pad,
                    available: payload.len(),
                });
            }
            let keep = payload.len().saturating_sub(pad);
            payload = payload.get(..keep).ok_or(RtpError::BadPadding {
                declared: pad,
                available: payload.len(),
            })?;
        }

        Ok(Self {
            header: RtpHeader {
                marker,
                payload_type,
                sequence,
                timestamp,
                ssrc,
                csrc_count,
                has_extension,
                extension_profile,
            },
            payload,
        })
    }
}

/// Read a big-endian `u16` at `offset`, or [`RtpError::TooShort`].
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, RtpError> {
    let end = offset.checked_add(2).ok_or(RtpError::TooShort {
        need: usize::MAX,
        got: bytes.len(),
    })?;
    let slice = bytes.get(offset..end).ok_or(RtpError::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let hi = *slice.first().ok_or(RtpError::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let lo = *slice.get(1).ok_or(RtpError::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}

/// Read a big-endian `u32` at `offset`, or [`RtpError::TooShort`].
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, RtpError> {
    let hi = read_u16(bytes, offset)?;
    let lo = read_u16(bytes, offset.saturating_add(2))?;
    Ok((u32::from(hi) << 16) | u32::from(lo))
}

/// Compute the **forward distance** from sequence `a` to sequence `b` in 16-bit
/// RTP sequence space, treating wrap-around as continuation.
///
/// Returns `b - a` interpreted as the shortest forward step modulo 2^16, i.e.
/// the number of sequence increments to advance from `a` to `b`. Equal numbers
/// yield `0`; one-ahead yields `1`; the value never panics and never overflows
/// (the subtraction is done in wrapping `u16` arithmetic).
#[must_use]
pub fn seq_distance(a: u16, b: u16) -> u16 {
    b.wrapping_sub(a)
}

/// Whether sequence `b` is *after* sequence `a` in RTP sequence space, using the
/// RFC 1982 serial-number comparison (a difference in the open lower half is
/// "after", the upper half is "before").
#[must_use]
pub fn seq_after(a: u16, b: u16) -> bool {
    let d = b.wrapping_sub(a);
    d != 0 && d < 0x8000
}
