//! Capsule protocol (RFC 9297 section 3.2) on the WebTransport CONNECT
//! stream: a bounded streaming parser for the bytes carried in its DATA
//! frames, and the `CLOSE_WEBTRANSPORT_SESSION` encoder.
//!
//! Bounds: a capsule we act on (`CLOSE_WEBTRANSPORT_SESSION`, DRAIN) is
//! buffered only up to [`MAX_KNOWN_CAPSULE`] bytes; any other capsule type is
//! skipped by counting and never buffered. A CLOSE capsule declaring more is
//! a protocol error.

use crate::varint;

/// `CLOSE_WEBTRANSPORT_SESSION` (draft-ietf-webtrans-http3).
pub const CLOSE_WEBTRANSPORT_SESSION: u64 = 0x2843;
/// `DRAIN_WEBTRANSPORT_SESSION`.
pub const DRAIN_WEBTRANSPORT_SESSION: u64 = 0x78ae;
/// The longest close reason, in bytes (draft-ietf-webtrans-http3).
pub const MAX_REASON: usize = 1024;
/// The largest capsule body this parser buffers: a 4-byte code plus a
/// maximal reason.
pub const MAX_KNOWN_CAPSULE: u64 = 4 + 1024;

/// A capsule the session acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capsule {
    /// The peer closed the session with an application code and reason.
    Close {
        /// Application error code.
        code: u32,
        /// UTF-8 reason (lossy-decoded), at most [`MAX_REASON`] bytes.
        reason: String,
    },
    /// The peer asked the session to drain.
    Drain,
}

/// A capsule stream that cannot be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapsuleError {
    /// A CLOSE/DRAIN capsule declared a body larger than allowed.
    TooLarge(u64),
    /// A CLOSE capsule shorter than its 4-byte code.
    Truncated,
}

impl std::fmt::Display for CapsuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(len) => write!(f, "capsule body of {len} bytes exceeds the limit"),
            Self::Truncated => f.write_str("close capsule shorter than its error code"),
        }
    }
}

impl std::error::Error for CapsuleError {}

/// Streaming capsule parser. Holds at most 16 header bytes plus one known
/// capsule body of at most [`MAX_KNOWN_CAPSULE`] bytes.
#[derive(Debug, Default)]
pub struct CapsuleParser {
    head: Vec<u8>,
    /// Current capsule: (type, remaining body bytes).
    current: Option<(u64, u64)>,
    body: Vec<u8>,
}

impl CapsuleParser {
    /// A fresh parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes; pushes every complete known capsule to `out`.
    ///
    /// # Errors
    ///
    /// [`CapsuleError`] when a known capsule is oversized or malformed. The
    /// parser must not be fed after an error.
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<Capsule>) -> Result<(), CapsuleError> {
        loop {
            match self.current {
                None => {
                    let Some((&byte, rest)) = data.split_first() else {
                        return Ok(());
                    };
                    data = rest;
                    self.head.push(byte);
                    let Some((ty, ty_len)) = varint::decode(&self.head) else {
                        continue;
                    };
                    let Some((len, _)) =
                        varint::decode(self.head.get(ty_len..).unwrap_or_default())
                    else {
                        continue;
                    };
                    self.head.clear();
                    let known =
                        ty == CLOSE_WEBTRANSPORT_SESSION || ty == DRAIN_WEBTRANSPORT_SESSION;
                    if known && len > MAX_KNOWN_CAPSULE {
                        return Err(CapsuleError::TooLarge(len));
                    }
                    self.current = Some((ty, len));
                    self.body.clear();
                }
                Some((ty, remaining)) => {
                    if remaining > 0 {
                        if data.is_empty() {
                            return Ok(());
                        }
                        let available = u64::try_from(data.len()).unwrap_or(u64::MAX);
                        let step = remaining.min(available);
                        let step_usize = usize::try_from(step).unwrap_or(data.len());
                        let (take, rest) = data.split_at(step_usize.min(data.len()));
                        if ty == CLOSE_WEBTRANSPORT_SESSION || ty == DRAIN_WEBTRANSPORT_SESSION {
                            self.body.extend_from_slice(take);
                        }
                        data = rest;
                        self.current = Some((ty, remaining - step));
                        continue;
                    }
                    self.current = None;
                    match ty {
                        CLOSE_WEBTRANSPORT_SESSION => {
                            let Some((code, reason)) = self.body.split_first_chunk::<4>() else {
                                return Err(CapsuleError::Truncated);
                            };
                            let reason = reason
                                .get(..MAX_REASON.min(reason.len()))
                                .unwrap_or_default();
                            out.push(Capsule::Close {
                                code: u32::from_be_bytes(*code),
                                reason: String::from_utf8_lossy(reason).into_owned(),
                            });
                        }
                        DRAIN_WEBTRANSPORT_SESSION => out.push(Capsule::Drain),
                        _ => {}
                    }
                    self.body.clear();
                }
            }
        }
    }
}

/// Truncates `reason` to at most [`MAX_REASON`] bytes on a char boundary.
#[must_use]
pub fn truncate_reason(reason: &str) -> &str {
    if reason.len() <= MAX_REASON {
        return reason;
    }
    let mut end = MAX_REASON;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason.get(..end).unwrap_or_default()
}

/// Encodes a `CLOSE_WEBTRANSPORT_SESSION` capsule (reason truncated to
/// [`MAX_REASON`] bytes).
#[must_use]
pub fn encode_close(code: u32, reason: &str) -> Vec<u8> {
    let reason = truncate_reason(reason);
    let mut out = Vec::with_capacity(16 + reason.len());
    varint::encode(CLOSE_WEBTRANSPORT_SESSION, &mut out);
    varint::encode(4 + u64::try_from(reason.len()).unwrap_or(0), &mut out);
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(reason.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_round_trips_split_anywhere() {
        let mut wire = Vec::new();
        // An unknown capsule first, skipped.
        varint::encode(0x29, &mut wire);
        varint::encode(3, &mut wire);
        wire.extend_from_slice(b"xyz");
        wire.extend(encode_close(7, "bye"));
        for split in 0..wire.len() {
            let mut parser = CapsuleParser::new();
            let mut out = Vec::new();
            let (a, b) = wire.split_at(split);
            parser.feed(a, &mut out).unwrap();
            parser.feed(b, &mut out).unwrap();
            assert_eq!(
                out,
                vec![Capsule::Close {
                    code: 7,
                    reason: "bye".into()
                }]
            );
        }
    }

    #[test]
    fn reason_is_truncated_and_oversized_close_rejected() {
        let long = "é".repeat(1000);
        let wire = encode_close(1, &long);
        let mut out = Vec::new();
        CapsuleParser::new().feed(&wire, &mut out).unwrap();
        let Some(Capsule::Close { reason, .. }) = out.first() else {
            panic!("no close");
        };
        assert!(reason.len() <= MAX_REASON);

        let mut bad = Vec::new();
        varint::encode(CLOSE_WEBTRANSPORT_SESSION, &mut bad);
        varint::encode(1 << 20, &mut bad);
        assert_eq!(
            CapsuleParser::new().feed(&bad, &mut Vec::new()),
            Err(CapsuleError::TooLarge(1 << 20))
        );
    }
}
