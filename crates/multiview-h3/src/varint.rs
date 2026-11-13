//! QUIC variable-length integers (RFC 9000 section 16) over plain byte
//! slices, for the sniffers that look at stream bytes before h3 does.

/// The largest value a QUIC varint can carry (2^62 - 1).
pub const MAX: u64 = (1 << 62) - 1;

/// Decodes one varint from the front of `buf`.
///
/// Returns `None` when `buf` holds fewer bytes than the varint needs, else
/// the value and the number of bytes it occupied.
#[must_use]
pub fn decode(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    let bytes = buf.get(..len)?;
    let mut value = u64::from(first & 0x3f);
    for byte in bytes.iter().skip(1) {
        value = (value << 8) | u64::from(*byte);
    }
    Some((value, len))
}

/// The number of bytes `value` takes as a varint, or `None` above [`MAX`].
#[must_use]
pub const fn size(value: u64) -> Option<usize> {
    if value < (1 << 6) {
        Some(1)
    } else if value < (1 << 14) {
        Some(2)
    } else if value < (1 << 30) {
        Some(4)
    } else if value <= MAX {
        Some(8)
    } else {
        None
    }
}

/// Appends `value` as a varint. Values above [`MAX`] are clamped to it (no
/// caller in this crate encodes such a value; the clamp keeps this total).
pub fn encode(value: u64, out: &mut Vec<u8>) {
    let value = value.min(MAX);
    let (tagged, len) = match size(value) {
        Some(1) => (value, 1),
        Some(2) => (value | (0b01 << 14), 2),
        Some(4) => (value | (0b10 << 30), 4),
        _ => (value | (0b11 << 62), 8),
    };
    out.extend(tagged.to_be_bytes().iter().skip(8 - len));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_length_class() {
        for value in [0, 63, 64, 16_383, 16_384, (1 << 30) - 1, 1 << 30, MAX] {
            let mut out = Vec::new();
            encode(value, &mut out);
            assert_eq!(Some(out.len()), size(value));
            assert_eq!(decode(&out), Some((value, out.len())));
            assert_eq!(decode(&out[..out.len() - 1]), None);
        }
        assert_eq!(decode(&[]), None);
    }
}
