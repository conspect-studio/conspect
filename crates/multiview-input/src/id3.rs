//! Bounded, pure ID3v2.3/v2.4 timed metadata decoding.
//!
//! Raw frames remain evidence, never inferred ad/break boundaries. Apple PRIV
//! timestamps are raw 90 kHz, 33-bit transport evidence, not normalized time.
//! Unsupported transformations reject the whole tag; no partial result escapes.

/// Maximum declared tag body, including frame headers and padding (64 KiB).
pub const MAX_TAG_BYTES: usize = 65_536;
/// Maximum frames retained per tag.
pub const MAX_FRAMES: usize = 256;
/// Maximum payload of a single frame (16 KiB).
pub const MAX_FRAME_PAYLOAD_BYTES: usize = 16_384;

/// A validated tag. No event/break semantics are assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Id3Tag {
    /// ID3 major version (3 or 4).
    pub version: u8,
    /// Frames in wire order, including unknown frame IDs.
    pub frames: Vec<Id3Frame>,
}

/// Raw metadata plus narrowly recognized timestamp evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Id3Frame {
    /// Four ASCII uppercase/digit frame identifier bytes.
    pub id: [u8; 4],
    /// Original status and format flags (big endian).
    pub flags: u16,
    /// Exact untransformed wire payload.
    pub payload: Vec<u8>,
    /// Apple transport timestamp, if present, without unwrapping or normalization.
    pub transport_timestamp_90khz: Option<u64>,
}

/// Fail-closed parse diagnostics; errors never include a partially trusted tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id3Error {
    /// Header or declared frame payload is truncated.
    Truncated,
    /// Invalid ID3 magic or revision, or unsupported major version.
    InvalidHeader,
    /// Non-synchsafe size, overflow, or bytes outside the declared tag.
    InvalidSize,
    /// Tag, frame count, or payload exceeds a fixed resource bound.
    TooLarge,
    /// Unsupported tag/frame feature or reserved flag bit.
    UnsupportedFlags,
    /// Invalid frame ID, zero size, padding, or malformed PRIV owner.
    InvalidFrame,
    /// Apple timestamp is not exactly eight bytes with only 33 bits set.
    InvalidTimestamp,
}

impl std::fmt::Display for Id3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ID3 parse error: {self:?}")
    }
}
impl std::error::Error for Id3Error {}

fn synchsafe(bytes: &[u8]) -> Result<usize, Id3Error> {
    let mut size = 0_usize;
    for &byte in bytes {
        if byte & 0x80 != 0 {
            return Err(Id3Error::InvalidSize);
        }
        size = size
            .checked_mul(128)
            .and_then(|n| n.checked_add(usize::from(byte)))
            .ok_or(Id3Error::InvalidSize)?;
    }
    Ok(size)
}

/// Parse exactly one complete tag, with no trailing transport bytes.
///
/// Accepts revision-zero v2.3/v2.4 tags and zero padding. Extended headers,
/// footers, unsynchronization, compression, encryption, grouping and data-length
/// indicators are deliberately unsupported. Status-only frame flags and the
/// experimental tag flag are preserved/accepted; reserved bits are rejected.
/// Bounds are checked before payload allocation. This function performs no I/O.
///
/// # Errors
/// Returns an error for malformed/truncated data, unsupported features or limits.
pub fn parse(bytes: &[u8]) -> Result<Id3Tag, Id3Error> {
    let &[a, b, c, version, revision, flags, s0, s1, s2, s3] =
        bytes.first_chunk::<10>().ok_or(Id3Error::Truncated)?;
    if [a, b, c] != *b"ID3" || !matches!(version, 3 | 4) || revision != 0 {
        return Err(Id3Error::InvalidHeader);
    }
    if flags & !0x20 != 0 {
        return Err(Id3Error::UnsupportedFlags);
    }
    let size = synchsafe(&[s0, s1, s2, s3])?;
    if size > MAX_TAG_BYTES {
        return Err(Id3Error::TooLarge);
    }
    let end = size.checked_add(10).ok_or(Id3Error::InvalidSize)?;
    if bytes.len() < end {
        return Err(Id3Error::Truncated);
    }
    if bytes.len() != end {
        return Err(Id3Error::InvalidSize);
    }
    let mut remaining = bytes.get(10..end).ok_or(Id3Error::Truncated)?;
    let mut frames = Vec::new();
    while !remaining.is_empty() {
        if remaining.first() == Some(&0) {
            if remaining.iter().any(|&byte| byte != 0) {
                return Err(Id3Error::InvalidFrame);
            }
            break;
        }
        if frames.len() >= MAX_FRAMES {
            return Err(Id3Error::TooLarge);
        }
        let &[a, b, c, d, s0, s1, s2, s3, status, format] =
            remaining.first_chunk::<10>().ok_or(Id3Error::Truncated)?;
        let id = [a, b, c, d];
        if !id
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return Err(Id3Error::InvalidFrame);
        }
        let allowed_status = if version == 3 { 0xe0 } else { 0x70 };
        if status & !allowed_status != 0 || format != 0 {
            return Err(Id3Error::UnsupportedFlags);
        }
        let payload_size = if version == 4 {
            synchsafe(&[s0, s1, s2, s3])?
        } else {
            usize::try_from(u32::from_be_bytes([s0, s1, s2, s3]))
                .map_err(|_| Id3Error::InvalidSize)?
        };
        if payload_size == 0 {
            return Err(Id3Error::InvalidFrame);
        }
        if payload_size > MAX_FRAME_PAYLOAD_BYTES {
            return Err(Id3Error::TooLarge);
        }
        let frame_end = payload_size.checked_add(10).ok_or(Id3Error::InvalidSize)?;
        let payload = remaining.get(10..frame_end).ok_or(Id3Error::Truncated)?;
        let timestamp = if &id == b"PRIV" {
            private_timestamp(payload)?
        } else {
            None
        };
        frames.push(Id3Frame {
            id,
            flags: u16::from_be_bytes([status, format]),
            payload: payload.to_vec(),
            transport_timestamp_90khz: timestamp,
        });
        remaining = remaining.get(frame_end..).ok_or(Id3Error::Truncated)?;
    }
    Ok(Id3Tag { version, frames })
}

fn private_timestamp(payload: &[u8]) -> Result<Option<u64>, Id3Error> {
    let owner_end = payload
        .iter()
        .position(|&b| b == 0)
        .ok_or(Id3Error::InvalidFrame)?;
    if payload.get(..owner_end).ok_or(Id3Error::InvalidFrame)?
        != b"com.apple.streaming.transportStreamTimestamp"
    {
        return Ok(None);
    }
    let raw: [u8; 8] = payload
        .get(owner_end + 1..)
        .ok_or(Id3Error::InvalidFrame)?
        .try_into()
        .map_err(|_| Id3Error::InvalidTimestamp)?;
    let timestamp = u64::from_be_bytes(raw);
    if timestamp >= (1_u64 << 33) {
        return Err(Id3Error::InvalidTimestamp);
    }
    Ok(Some(timestamp))
}
