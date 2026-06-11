//! SMPTE **ST 2022-6** HBRMT (High Bit-Rate Media Transport) framing parser
//! (pure).
//!
//! ST 2022-6 carries a whole SDI signal over RTP by prefixing each RTP payload
//! with a fixed **8-byte HBRMT payload header** (the "High Bit-Rate Media
//! Transport" header) ahead of the SDI octets:
//!
//! ```text
//!  0                   1                   2                   3
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! | Ext  |F| VSID|   FRCount     |R|S| FEC |CF |   reserved      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! | MAP |FRAME| FRATE | SAMPLE  |     FMT-Reserve / reserved      |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                        Video timestamp (optional)             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                       SDI payload octets ...                  |
//! ```
//!
//! Only the high-level framing is decoded here — the header word fields that
//! identify the video standard (`MAP`, `FRAME`, `FRATE`, `SAMPLE`), the
//! frame-count, and the optional 32-bit video timestamp — plus a borrowed view
//! of the SDI payload. It is a **pure**, panic-free byte-slice parser; turning
//! the SDI octets into pixels is a downstream concern.

/// The fixed HBRMT payload-header length, in bytes (without the optional video
/// timestamp word).
pub const HBRMT_HEADER_LEN: usize = 8;

/// The HBRMT header length when the optional 4-byte video timestamp is present.
pub const HBRMT_HEADER_LEN_WITH_TS: usize = 12;

/// Errors raised while parsing an ST 2022-6 HBRMT payload.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum St2022_6Error {
    /// The payload was shorter than the HBRMT header requires.
    #[error("st2022-6 payload too short: need at least {need} bytes, got {got}")]
    TooShort {
        /// Minimum bytes the parser required.
        need: usize,
        /// Bytes actually supplied.
        got: usize,
    },
}

/// The video-standard descriptor fields decoded from the HBRMT header's second
/// word (`MAP` / `FRAME` / `FRATE` / `SAMPLE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HbrmtVideoFormat {
    /// The `MAP` field (SDI mapping, e.g. direct / level-A / level-B).
    pub map: u8,
    /// The `FRAME` field (raster: 1080i, 720p, 1080p, …).
    pub frame: u8,
    /// The `FRATE` field (frame rate code).
    pub frate: u8,
    /// The `SAMPLE` field (sampling structure / colour depth).
    pub sample: u8,
}

/// A parsed ST 2022-6 HBRMT payload: the framing fields plus the SDI payload
/// view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hbrmt<'a> {
    /// The frame counter (`FRCount`) — increments once per video frame.
    pub frame_count: u8,
    /// Whether the optional video-timestamp word is present (the `S` /
    /// clock-frequency flag indicates a media-clock timestamp follows).
    pub has_video_timestamp: bool,
    /// The 32-bit video timestamp, present only when [`has_video_timestamp`] is
    /// set.
    ///
    /// [`has_video_timestamp`]: Hbrmt::has_video_timestamp
    pub video_timestamp: Option<u32>,
    /// The decoded video-standard descriptor.
    pub format: HbrmtVideoFormat,
    /// The SDI payload octets following the HBRMT header.
    pub sdi: &'a [u8],
}

impl<'a> Hbrmt<'a> {
    /// Parse an ST 2022-6 RTP **payload** (the bytes after the RTP fixed
    /// header).
    ///
    /// # Errors
    ///
    /// [`St2022_6Error::TooShort`] if the payload cannot hold the HBRMT header
    /// (plus the video-timestamp word, when the clock-frequency flag is set).
    pub fn parse(payload: &'a [u8]) -> Result<Self, St2022_6Error> {
        if payload.len() < HBRMT_HEADER_LEN {
            return Err(St2022_6Error::TooShort {
                need: HBRMT_HEADER_LEN,
                got: payload.len(),
            });
        }
        // Byte 0: Ext(4) | F(1) | VSID(3). Byte 1: FRCount. Byte 2: R|S|FEC|CF.
        let b2 = *payload.get(2).ok_or(St2022_6Error::TooShort {
            need: 3,
            got: payload.len(),
        })?;
        // The `S` bit (bit 6) signals that a 32-bit video timestamp word is
        // appended to the header.
        let has_video_timestamp = (b2 & 0b0100_0000) != 0;

        let frame_count = *payload.get(1).ok_or(St2022_6Error::TooShort {
            need: 2,
            got: payload.len(),
        })?;

        // Bytes 4..6 carry MAP|FRAME|FRATE|SAMPLE across two octets:
        // byte 4 = MAP(4)|FRAME(4); byte 5 = FRATE(4)|SAMPLE(4).
        let b4 = *payload.get(4).ok_or(St2022_6Error::TooShort {
            need: 5,
            got: payload.len(),
        })?;
        let b5 = *payload.get(5).ok_or(St2022_6Error::TooShort {
            need: 6,
            got: payload.len(),
        })?;
        let format = HbrmtVideoFormat {
            map: b4 >> 4,
            frame: b4 & 0x0F,
            frate: b5 >> 4,
            sample: b5 & 0x0F,
        };

        let header_len = if has_video_timestamp {
            HBRMT_HEADER_LEN_WITH_TS
        } else {
            HBRMT_HEADER_LEN
        };
        if payload.len() < header_len {
            return Err(St2022_6Error::TooShort {
                need: header_len,
                got: payload.len(),
            });
        }
        let video_timestamp = if has_video_timestamp {
            Some(read_u32(payload, HBRMT_HEADER_LEN)?)
        } else {
            None
        };

        let sdi = payload.get(header_len..).ok_or(St2022_6Error::TooShort {
            need: header_len,
            got: payload.len(),
        })?;

        Ok(Self {
            frame_count,
            has_video_timestamp,
            video_timestamp,
            format,
            sdi,
        })
    }
}

/// Read a big-endian `u32` at `offset`, or [`St2022_6Error::TooShort`].
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, St2022_6Error> {
    let end = offset.checked_add(4).ok_or(St2022_6Error::TooShort {
        need: usize::MAX,
        got: bytes.len(),
    })?;
    let slice = bytes.get(offset..end).ok_or(St2022_6Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let b0 = *slice.first().ok_or(St2022_6Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let b1 = *slice.get(1).ok_or(St2022_6Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let b2 = *slice.get(2).ok_or(St2022_6Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let b3 = *slice.get(3).ok_or(St2022_6Error::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    Ok(u32::from_be_bytes([b0, b1, b2, b3]))
}
