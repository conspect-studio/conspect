//! SMPTE **ST 2110-30** (AES67) PCM-audio RTP payload depacketizer (pure).
//!
//! ST 2110-30 carries linear PCM exactly as AES67 (RFC 3190 / L16, L24): the RTP
//! payload is a sequence of **audio sample groups**, each group holding one
//! sample for every channel, big-endian, in channel order. There is no
//! per-payload header beyond the RTP header itself — the channel count and
//! sample depth come from the SDP-negotiated format, which the caller supplies
//! as an [`Aes3Format`].
//!
//! This module is a **pure** byte-slice → typed value parser: it validates that
//! the payload is a whole number of sample groups for the configured format and
//! exposes per-channel / per-group accessors over the borrowed bytes. It never
//! allocates and never panics on malformed input.

/// The PCM sample depth carried by an ST 2110-30 / AES67 stream.
///
/// `#[non_exhaustive]`: AES67 also permits other depths (e.g. L8); the two
/// professional depths are modelled here and a wildcard arm keeps future
/// additions non-breaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SampleDepth {
    /// 16-bit signed PCM (AES67 "L16"), 2 bytes per sample.
    L16,
    /// 24-bit signed PCM (AES67 "L24"), 3 bytes per sample.
    L24,
}

impl SampleDepth {
    /// The number of bytes one sample of this depth occupies on the wire.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            SampleDepth::L16 => 2,
            SampleDepth::L24 => 3,
        }
    }
}

/// The SDP-negotiated PCM format an ST 2110-30 stream is carried in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Aes3Format {
    /// Number of audio channels per sample group (`1..`).
    pub channels: u8,
    /// PCM sample depth.
    pub depth: SampleDepth,
}

impl Aes3Format {
    /// Construct a format, rejecting a zero channel count.
    ///
    /// # Errors
    ///
    /// [`V30Error::ZeroChannels`] if `channels` is `0` — a stream must carry at
    /// least one channel.
    pub fn new(channels: u8, depth: SampleDepth) -> Result<Self, V30Error> {
        if channels == 0 {
            return Err(V30Error::ZeroChannels);
        }
        Ok(Self { channels, depth })
    }

    /// The size, in bytes, of one sample group (one sample for every channel).
    #[must_use]
    pub fn group_bytes(self) -> usize {
        // `channels` is `1..=255`; the product cannot overflow `usize`.
        usize::from(self.channels) * self.depth.bytes_per_sample()
    }
}

/// Errors raised while depacketizing an ST 2110-30 RTP payload.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum V30Error {
    /// A format was constructed with zero channels.
    #[error("st2110-30 format must have at least one channel")]
    ZeroChannels,

    /// The payload length was not a whole number of sample groups for the
    /// configured format (a partial group is a framing error).
    #[error("st2110-30 payload {payload} bytes is not a multiple of group size {group}")]
    PartialGroup {
        /// The payload length in bytes.
        payload: usize,
        /// The size of one sample group in bytes.
        group: usize,
    },
}

/// A depacketized ST 2110-30 audio payload: its format plus a borrowed view of
/// the interleaved PCM groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V30Payload<'a> {
    /// The PCM format the payload was parsed against.
    pub format: Aes3Format,
    /// The interleaved sample-group bytes (a whole number of groups).
    pub data: &'a [u8],
}

impl<'a> V30Payload<'a> {
    /// Parse an ST 2110-30 RTP payload for the given [`Aes3Format`].
    ///
    /// Validates that the payload is a whole number of sample groups and returns
    /// a borrowed view; per-channel samples are read lazily via
    /// [`V30Payload::sample`].
    ///
    /// # Errors
    ///
    /// [`V30Error::PartialGroup`] if the payload length is not a multiple of the
    /// format's group size.
    pub fn parse(payload: &'a [u8], format: Aes3Format) -> Result<Self, V30Error> {
        let group = format.group_bytes();
        if group == 0 || payload.len() % group != 0 {
            return Err(V30Error::PartialGroup {
                payload: payload.len(),
                group,
            });
        }
        Ok(Self {
            format,
            data: payload,
        })
    }

    /// The number of complete sample groups (samples per channel) in the
    /// payload.
    #[must_use]
    pub fn group_count(&self) -> usize {
        let group = self.format.group_bytes();
        self.data.len().checked_div(group).unwrap_or(0)
    }

    /// Read the signed sample value for `channel` (`0`-based) in sample group
    /// `group_index`, sign-extended from its wire depth into an `i32`.
    ///
    /// Returns [`None`] if either index is out of range.
    #[must_use]
    pub fn sample(&self, group_index: usize, channel: usize) -> Option<i32> {
        if channel >= usize::from(self.format.channels) {
            return None;
        }
        let bps = self.format.depth.bytes_per_sample();
        let group = self.format.group_bytes();
        let start = group_index
            .checked_mul(group)?
            .checked_add(channel.checked_mul(bps)?)?;
        let end = start.checked_add(bps)?;
        let bytes = self.data.get(start..end)?;
        match self.format.depth {
            SampleDepth::L16 => {
                let hi = *bytes.first()?;
                let lo = *bytes.get(1)?;
                // Sign-extend a 16-bit big-endian sample via `i16::from_be_bytes`
                // (no `as` cast), then widen losslessly to `i32`.
                Some(i32::from(i16::from_be_bytes([hi, lo])))
            }
            SampleDepth::L24 => {
                let hi = *bytes.first()?;
                let mid = *bytes.get(1)?;
                let lo = *bytes.get(2)?;
                // Sign-extend a 24-bit big-endian sample: place the three octets
                // in the high three bytes of an `i32` then arithmetic-shift down,
                // all without an `as` cast.
                let widened = i32::from_be_bytes([hi, mid, lo, 0]);
                Some(widened >> 8)
            }
        }
    }
}
