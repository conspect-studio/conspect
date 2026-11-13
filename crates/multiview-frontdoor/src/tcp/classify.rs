//! TCP first-byte classification (design note section 4).
//!
//! The door decides a connection's protocol from at most its first two bytes
//! and reads nothing else before the decision: `0x16` is a TLS handshake
//! record; a 16-bit big-endian RFC 4571 length of 20..=1535 (first byte
//! `0x00`-`0x05`) is an ICE-TCP connection's first STUN frame; anything else
//! (the `G` of a plaintext `GET`, a PROXY header in direct mode) is closed.

/// The smallest first ICE-TCP frame: a STUN header (RFC 8489 section 5).
pub const MIN_FIRST_ICE_FRAME: u16 = 20;
/// The largest first (unauthenticated) ICE-TCP frame (design note section 4).
pub const MAX_FIRST_ICE_FRAME: u16 = 1535;

/// What the first bytes of a TCP connection say it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpClass {
    /// A TLS handshake record (`0x16`).
    Tls,
    /// An RFC 4571 frame whose length is a plausible first STUN message.
    IceTcp {
        /// The declared frame length.
        len: u16,
    },
    /// One more byte is needed to decide.
    NeedMore,
    /// Not ours: close.
    Close,
}

/// Classify the bytes seen so far (pure; at most two are looked at).
#[must_use]
pub fn classify_first_bytes(bytes: &[u8]) -> TcpClass {
    match bytes {
        [] | [0x00..=0x05] => TcpClass::NeedMore,
        [0x16, ..] => TcpClass::Tls,
        [hi @ 0x00..=0x05, lo, ..] => {
            let len = u16::from_be_bytes([*hi, *lo]);
            if (MIN_FIRST_ICE_FRAME..=MAX_FIRST_ICE_FRAME).contains(&len) {
                TcpClass::IceTcp { len }
            } else {
                TcpClass::Close
            }
        }
        _ => TcpClass::Close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_first_byte_has_exactly_one_class() {
        for byte in 0..=u8::MAX {
            let class = classify_first_bytes(&[byte]);
            let expected = match byte {
                0x16 => TcpClass::Tls,
                0x00..=0x05 => TcpClass::NeedMore,
                _ => TcpClass::Close,
            };
            assert_eq!(class, expected, "first byte {byte:#04x}");
        }
        assert_eq!(classify_first_bytes(&[]), TcpClass::NeedMore);
    }

    #[test]
    fn the_length_prefix_bounds_the_first_ice_frame() {
        for len in 0..=u16::MAX {
            let [hi, lo] = len.to_be_bytes();
            let class = classify_first_bytes(&[hi, lo, 0]);
            if hi > 0x05 {
                let expected = if hi == 0x16 {
                    TcpClass::Tls
                } else {
                    TcpClass::Close
                };
                assert_eq!(class, expected, "{len}");
            } else if (20..=1535).contains(&len) {
                assert_eq!(class, TcpClass::IceTcp { len });
            } else {
                assert_eq!(class, TcpClass::Close, "{len}");
            }
        }
    }

    #[test]
    fn plaintext_and_proxy_headers_close() {
        assert_eq!(classify_first_bytes(b"GET / HTTP/1.1"), TcpClass::Close);
        assert_eq!(classify_first_bytes(b"PROXY TCP4"), TcpClass::Close);
        assert_eq!(
            classify_first_bytes(b"\r\n\r\n\0\r\nQUIT\n"),
            TcpClass::Close
        );
    }
}
