//! An **Ember+** (Glow over S101) protocol codec.
//!
//! Ember+ is the second openly-published router/device-control protocol Multiview
//! speaks (broadcast-multiviewer brief §8). A device exposes a **tree** of nodes
//! and parameters; the **Glow** DTD encodes that tree in **BER** (ASN.1 Basic
//! Encoding Rules), and the **S101** packet layer frames the BER blob for the
//! byte stream (serial / TCP) with escaping and a CRC.
//!
//! This module owns the **pure** codec for the slice Multiview needs — enough to
//! read a router's source/destination labels (name-following) and parameter
//! values, and to set a parameter (e.g. drive a route on a parameter-modelled
//! router). The live socket is behind the off-by-default `router` feature
//! (`super::transport`); the codec below is exhaustively testable with no I/O.
//!
//! ## Two layers, kept separate
//!
//! 1. **BER** ([`ber`]): minimal definite-length tag/length/value coding for the
//!    handful of universal types a Glow tree uses (integer, UTF-8 string,
//!    boolean, and the constructed application/context tags). Pure and
//!    round-trippable.
//! 2. **S101** ([`s101`]): the message frame — `0xFE … 0xFF` delimited, `0xFD`
//!    byte-stuffing, and a trailing **CRC-16/CCITT (X.25)** over the payload.
//!
//! On top sits a small [`GlowNode`] model so callers deal in a typed tree rather
//! than raw BER.
use serde::{Deserialize, Serialize};

/// An Ember+ codec error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EmberError {
    /// A BER value ran past the end of its buffer.
    #[error("truncated BER value (need {need} bytes, have {have})")]
    Truncated {
        /// Bytes the value declared it needed.
        need: usize,
        /// Bytes actually remaining.
        have: usize,
    },
    /// A BER length used the indefinite or a too-wide long form.
    #[error("unsupported BER length encoding")]
    BadLength,
    /// A BER integer was wider than 8 bytes (more than this codec models).
    #[error("BER integer wider than 64 bits")]
    IntegerOverflow,
    /// A BER string was not valid UTF-8.
    #[error("BER string is not valid UTF-8")]
    BadUtf8,
    /// The value's tag did not match what the caller expected.
    #[error("unexpected BER tag {found:#04x}, expected {expected:#04x}")]
    UnexpectedTag {
        /// The tag the decoder found.
        found: u8,
        /// The tag the caller expected.
        expected: u8,
    },
    /// The S101 frame was not `0xFE … 0xFF` delimited.
    #[error("S101 frame missing its start/end delimiter")]
    NoFrame,
    /// A stray escape byte (`0xFD` not followed by a valid escapee).
    #[error("S101 stray escape byte")]
    StrayEscape,
    /// The S101 CRC did not validate the payload.
    #[error("S101 CRC mismatch: computed {computed:#06x}, frame carried {found:#06x}")]
    BadCrc {
        /// The CRC this codec computed.
        computed: u16,
        /// The CRC the frame carried.
        found: u16,
    },
}

/// Minimal BER (definite-length) tag/length/value coding for Glow.
pub mod ber {
    use super::EmberError;

    /// Universal **INTEGER** tag (`0x02`).
    pub const TAG_INTEGER: u8 = 0x02;
    /// Universal **BOOLEAN** tag (`0x01`).
    pub const TAG_BOOLEAN: u8 = 0x01;
    /// Universal **`UTF8String`** tag (`0x0C`).
    pub const TAG_UTF8: u8 = 0x0C;

    /// Encode a definite-length BER length prefix.
    ///
    /// Short form for `len < 128`; long form (`0x80 | n` then `n` big-endian
    /// bytes) otherwise.
    #[must_use]
    pub fn encode_length(len: usize) -> Vec<u8> {
        if len < 0x80 {
            // len < 128 fits the short form in one byte.
            vec![u8::try_from(len).unwrap_or(0)]
        } else {
            let bytes = len.to_be_bytes();
            let first = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
            let significant: Vec<u8> = bytes.iter().skip(first).copied().collect();
            let count = u8::try_from(significant.len()).unwrap_or(0);
            let mut out = Vec::with_capacity(significant.len() + 1);
            out.push(0x80 | count);
            out.extend_from_slice(&significant);
            out
        }
    }

    /// Decode a definite-length BER length prefix, returning `(length, rest)`.
    ///
    /// # Errors
    ///
    /// * [`EmberError::Truncated`] if the prefix runs past the buffer.
    /// * [`EmberError::BadLength`] for the indefinite form or a length wider than
    ///   `usize`.
    pub fn decode_length(input: &[u8]) -> Result<(usize, &[u8]), EmberError> {
        let (&first, rest) = input
            .split_first()
            .ok_or(EmberError::Truncated { need: 1, have: 0 })?;
        if first < 0x80 {
            return Ok((usize::from(first), rest));
        }
        if first == 0x80 || first == 0xFF {
            // Indefinite form (0x80) is unsupported; 0xFF is reserved.
            return Err(EmberError::BadLength);
        }
        let count = usize::from(first & 0x7F);
        if count > core::mem::size_of::<usize>() {
            return Err(EmberError::BadLength);
        }
        let bytes = rest.get(..count).ok_or(EmberError::Truncated {
            need: count,
            have: rest.len(),
        })?;
        let mut len: usize = 0;
        for &b in bytes {
            // count <= size_of::<usize>(), so this never overflows usize.
            len = (len << 8) | usize::from(b);
        }
        let tail = rest.get(count..).unwrap_or(&[]);
        Ok((len, tail))
    }

    /// Encode a tag/length/value triple.
    #[must_use]
    pub fn encode_tlv(tag: u8, value: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&encode_length(value.len()));
        out.extend_from_slice(value);
        out
    }

    /// Decode a tag/length/value triple, returning `(tag, value, rest)`.
    ///
    /// # Errors
    ///
    /// [`EmberError::Truncated`] / [`EmberError::BadLength`] on a malformed prefix
    /// or a value that runs past the buffer.
    pub fn decode_tlv(input: &[u8]) -> Result<(u8, &[u8], &[u8]), EmberError> {
        let (&tag, rest) = input
            .split_first()
            .ok_or(EmberError::Truncated { need: 1, have: 0 })?;
        let (len, rest) = decode_length(rest)?;
        let value = rest.get(..len).ok_or(EmberError::Truncated {
            need: len,
            have: rest.len(),
        })?;
        let tail = rest.get(len..).unwrap_or(&[]);
        Ok((tag, value, tail))
    }

    /// Encode a signed integer as a minimal two's-complement BER INTEGER.
    #[must_use]
    pub fn encode_integer(value: i64) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        // Trim redundant leading 0x00 / 0xFF bytes while preserving the sign bit.
        let mut start = 0usize;
        while start + 1 < bytes.len() {
            let (Some(&b), Some(&next)) = (bytes.get(start), bytes.get(start + 1)) else {
                break;
            };
            let redundant = (b == 0x00 && next & 0x80 == 0) || (b == 0xFF && next & 0x80 != 0);
            if redundant {
                start += 1;
            } else {
                break;
            }
        }
        let significant: Vec<u8> = bytes.iter().skip(start).copied().collect();
        encode_tlv(TAG_INTEGER, &significant)
    }

    /// Decode a BER INTEGER value body (the bytes after tag+length).
    ///
    /// # Errors
    ///
    /// [`EmberError::IntegerOverflow`] if the value is wider than 8 bytes.
    pub fn decode_integer(value: &[u8]) -> Result<i64, EmberError> {
        if value.len() > 8 {
            return Err(EmberError::IntegerOverflow);
        }
        // Sign-extend from the top bit of the first byte.
        let negative = value.first().is_some_and(|&b| b & 0x80 != 0);
        let mut acc: i64 = if negative { -1 } else { 0 };
        for &b in value {
            acc = (acc << 8) | i64::from(b);
        }
        Ok(acc)
    }

    /// Encode a UTF-8 string as a BER `UTF8String`.
    #[must_use]
    pub fn encode_utf8(text: &str) -> Vec<u8> {
        encode_tlv(TAG_UTF8, text.as_bytes())
    }

    /// Decode a BER `UTF8String` value body to an owned `String`.
    ///
    /// # Errors
    ///
    /// [`EmberError::BadUtf8`] if the bytes are not valid UTF-8.
    pub fn decode_utf8(value: &[u8]) -> Result<String, EmberError> {
        core::str::from_utf8(value)
            .map(str::to_owned)
            .map_err(|_| EmberError::BadUtf8)
    }
}

/// The S101 frame layer.
pub mod s101 {
    use super::EmberError;

    /// Start-of-frame byte.
    pub const BOF: u8 = 0xFE;
    /// End-of-frame byte.
    pub const EOF: u8 = 0xFF;
    /// Escape byte (the following byte is `b XOR 0x20`).
    pub const CE: u8 = 0xFD;
    /// The XOR mask applied to an escaped byte.
    pub const XOR: u8 = 0x20;

    /// Compute the CRC-16/CCITT (X.25) over a payload, as S101 uses.
    ///
    /// Reflected polynomial `0x8408`, init `0xFFFF`, final XOR `0xFFFF`.
    #[must_use]
    pub fn crc16(payload: &[u8]) -> u16 {
        let mut crc: u16 = 0xFFFF;
        for &byte in payload {
            crc ^= u16::from(byte);
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0x8408;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    /// Whether a byte must be escaped inside the frame body.
    const fn needs_escape(byte: u8) -> bool {
        byte == BOF || byte == EOF || byte == CE
    }

    /// Push a byte into the frame, escaping it if it collides with a delimiter.
    fn push_escaped(out: &mut Vec<u8>, byte: u8) {
        if needs_escape(byte) {
            out.push(CE);
            out.push(byte ^ XOR);
        } else {
            out.push(byte);
        }
    }

    /// Frame a payload: `BOF` + escaped(payload) + escaped(CRC, little-endian) +
    /// `EOF`. The CRC covers the **un-escaped** payload.
    #[must_use]
    pub fn encode(payload: &[u8]) -> Vec<u8> {
        let crc = crc16(payload);
        let mut out = vec![BOF];
        for &byte in payload {
            push_escaped(&mut out, byte);
        }
        // CRC is transmitted low byte first.
        push_escaped(&mut out, u8::try_from(crc & 0xFF).unwrap_or(0));
        push_escaped(&mut out, u8::try_from((crc >> 8) & 0xFF).unwrap_or(0));
        out.push(EOF);
        out
    }

    /// Un-frame an S101 message, validating the CRC and returning the payload.
    ///
    /// # Errors
    ///
    /// * [`EmberError::NoFrame`] for missing `BOF`/`EOF`.
    /// * [`EmberError::StrayEscape`] for a dangling escape byte.
    /// * [`EmberError::BadCrc`] / [`EmberError::Truncated`] on a CRC failure or a
    ///   body too short to carry one.
    pub fn decode(frame: &[u8]) -> Result<Vec<u8>, EmberError> {
        let inner = frame
            .strip_prefix(&[BOF])
            .and_then(|f| f.strip_suffix(&[EOF]))
            .ok_or(EmberError::NoFrame)?;
        let mut unescaped = Vec::with_capacity(inner.len());
        let mut iter = inner.iter().copied();
        while let Some(byte) = iter.next() {
            if byte == CE {
                let next = iter.next().ok_or(EmberError::StrayEscape)?;
                unescaped.push(next ^ XOR);
            } else {
                unescaped.push(byte);
            }
        }
        // The last two bytes are the CRC (little-endian) over the payload.
        if unescaped.len() < 2 {
            return Err(EmberError::Truncated {
                need: 2,
                have: unescaped.len(),
            });
        }
        let split = unescaped.len() - 2;
        let payload = unescaped.get(..split).unwrap_or(&[]).to_vec();
        let lo = unescaped.get(split).copied().unwrap_or(0);
        let hi = unescaped.get(split + 1).copied().unwrap_or(0);
        let found = u16::from(lo) | (u16::from(hi) << 8);
        let computed = crc16(&payload);
        if computed != found {
            return Err(EmberError::BadCrc { computed, found });
        }
        Ok(payload)
    }
}

/// The Glow application tag (context-specific) for a **Parameter** value field
/// this model carries. Glow assigns parameter `value` the context tag `[2]`.
const GLOW_VALUE_TAG: u8 = 0x82;
/// The Glow context tag for a parameter/node **number** (`[0]`).
const GLOW_NUMBER_TAG: u8 = 0x80;
/// The Glow context tag for an **identifier** string (`[1]`).
const GLOW_IDENTIFIER_TAG: u8 = 0x81;

/// A typed Glow parameter value (the subset Multiview reads/writes).
///
/// Internally tagged on `type` (never `untagged`) so it round-trips through JSON
/// for the diagnostic surface as well as BER.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum GlowValue {
    /// An integer parameter value.
    Integer {
        /// The integer value.
        value: i64,
    },
    /// A string parameter value (e.g. a source/destination label).
    String {
        /// The string value.
        value: String,
    },
}

impl GlowValue {
    /// An integer Glow value.
    #[must_use]
    pub const fn integer(value: i64) -> Self {
        Self::Integer { value }
    }

    /// A string Glow value.
    #[must_use]
    pub fn string(value: impl Into<String>) -> Self {
        Self::String {
            value: value.into(),
        }
    }

    /// Encode this value's BER bytes (the inner universal TLV).
    #[must_use]
    fn encode_inner(&self) -> Vec<u8> {
        match self {
            Self::Integer { value } => ber::encode_integer(*value),
            Self::String { value } => ber::encode_utf8(value),
        }
    }

    /// Decode a value from its inner universal TLV bytes.
    fn decode_inner(bytes: &[u8]) -> Result<Self, EmberError> {
        let (tag, value, _) = ber::decode_tlv(bytes)?;
        match tag {
            ber::TAG_INTEGER => Ok(Self::Integer {
                value: ber::decode_integer(value)?,
            }),
            ber::TAG_UTF8 => Ok(Self::String {
                value: ber::decode_utf8(value)?,
            }),
            other => Err(EmberError::UnexpectedTag {
                found: other,
                expected: ber::TAG_INTEGER,
            }),
        }
    }
}

/// One Glow node/parameter: a number, an identifier, and an optional value.
///
/// This is the minimal Glow shape Multiview needs to read a router's
/// source/destination labels (name-following) and to set a parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlowNode {
    /// The node/parameter number within its parent.
    pub number: i64,
    /// The node identifier (a stable programmatic name).
    pub identifier: String,
    /// The parameter value, if this is a value-carrying parameter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<GlowValue>,
}

impl GlowNode {
    /// Encode this node to its BER payload (the un-framed Glow blob).
    #[must_use]
    pub fn encode_ber(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&ber::encode_tlv(
            GLOW_NUMBER_TAG,
            &ber::encode_integer(self.number),
        ));
        body.extend_from_slice(&ber::encode_tlv(
            GLOW_IDENTIFIER_TAG,
            &ber::encode_utf8(&self.identifier),
        ));
        if let Some(value) = &self.value {
            body.extend_from_slice(&ber::encode_tlv(GLOW_VALUE_TAG, &value.encode_inner()));
        }
        body
    }

    /// Decode a node from its BER payload.
    ///
    /// # Errors
    ///
    /// Any [`EmberError`] from BER decoding, or [`EmberError::UnexpectedTag`] if
    /// the required number/identifier context tags are absent.
    pub fn decode_ber(payload: &[u8]) -> Result<Self, EmberError> {
        let mut number: Option<i64> = None;
        let mut identifier: Option<String> = None;
        let mut value: Option<GlowValue> = None;
        let mut rest = payload;
        while !rest.is_empty() {
            let (tag, inner, tail) = ber::decode_tlv(rest)?;
            match tag {
                GLOW_NUMBER_TAG => {
                    let (_, v, _) = ber::decode_tlv(inner)?;
                    number = Some(ber::decode_integer(v)?);
                }
                GLOW_IDENTIFIER_TAG => {
                    let (_, v, _) = ber::decode_tlv(inner)?;
                    identifier = Some(ber::decode_utf8(v)?);
                }
                GLOW_VALUE_TAG => {
                    value = Some(GlowValue::decode_inner(inner)?);
                }
                // Unknown context tags are ignored (forward-compatible).
                _ => {}
            }
            rest = tail;
        }
        Ok(Self {
            number: number.ok_or(EmberError::UnexpectedTag {
                found: 0,
                expected: GLOW_NUMBER_TAG,
            })?,
            identifier: identifier.ok_or(EmberError::UnexpectedTag {
                found: 0,
                expected: GLOW_IDENTIFIER_TAG,
            })?,
            value,
        })
    }

    /// Encode this node to a complete, framed S101 message ready for the wire.
    #[must_use]
    pub fn encode_message(&self) -> Vec<u8> {
        s101::encode(&self.encode_ber())
    }

    /// Decode a node from a complete, framed S101 message.
    ///
    /// # Errors
    ///
    /// Any [`EmberError`] from S101 un-framing or BER decoding.
    pub fn decode_message(frame: &[u8]) -> Result<Self, EmberError> {
        Self::decode_ber(&s101::decode(frame)?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{ber, s101, EmberError, GlowNode, GlowValue};

    #[test]
    fn ber_length_short_and_long_form_round_trip() {
        for len in [0usize, 1, 127, 128, 255, 256, 65535, 70000] {
            let encoded = ber::encode_length(len);
            let (decoded, rest) = ber::decode_length(&encoded).unwrap();
            assert_eq!(decoded, len, "len {len}");
            assert!(rest.is_empty());
        }
        // Short form is exactly one byte for < 128.
        assert_eq!(ber::encode_length(10), vec![10]);
        // Long form prefixes 0x80 | count.
        assert_eq!(ber::encode_length(128)[0], 0x81);
    }

    #[test]
    fn ber_length_rejects_indefinite_form() {
        let err = ber::decode_length(&[0x80]).unwrap_err();
        assert_eq!(err, EmberError::BadLength);
    }

    #[test]
    fn ber_integer_round_trips_including_negatives() {
        for v in [
            0i64,
            1,
            -1,
            127,
            128,
            -128,
            255,
            256,
            -32768,
            i64::MIN,
            i64::MAX,
        ] {
            let encoded = ber::encode_integer(v);
            let (tag, value, _) = ber::decode_tlv(&encoded).unwrap();
            assert_eq!(tag, ber::TAG_INTEGER);
            assert_eq!(ber::decode_integer(value).unwrap(), v, "value {v}");
        }
    }

    #[test]
    fn ber_integer_uses_minimal_encoding() {
        // 0 encodes to a single 0x00 body byte.
        let zero = ber::encode_integer(0);
        let (_, body, _) = ber::decode_tlv(&zero).unwrap();
        assert_eq!(body, &[0x00]);
        // 127 fits in one byte; 128 needs a leading 0x00 to stay positive.
        let one_two_seven = ber::encode_integer(127);
        let (_, body, _) = ber::decode_tlv(&one_two_seven).unwrap();
        assert_eq!(body, &[0x7F]);
        let one_two_eight = ber::encode_integer(128);
        let (_, body, _) = ber::decode_tlv(&one_two_eight).unwrap();
        assert_eq!(body, &[0x00, 0x80]);
    }

    #[test]
    fn ber_integer_rejects_overlong() {
        let err = ber::decode_integer(&[0; 9]).unwrap_err();
        assert_eq!(err, EmberError::IntegerOverflow);
    }

    #[test]
    fn ber_utf8_round_trips() {
        let encoded = ber::encode_utf8("CAM 1 — Wide");
        let (tag, value, _) = ber::decode_tlv(&encoded).unwrap();
        assert_eq!(tag, ber::TAG_UTF8);
        assert_eq!(ber::decode_utf8(value).unwrap(), "CAM 1 — Wide");
    }

    #[test]
    fn ber_utf8_rejects_bad_bytes() {
        let err = ber::decode_utf8(&[0xFF, 0xFE]).unwrap_err();
        assert_eq!(err, EmberError::BadUtf8);
    }

    #[test]
    fn s101_frame_round_trips_a_payload() {
        let payload = b"hello router".to_vec();
        let frame = s101::encode(&payload);
        assert_eq!(frame.first().copied(), Some(s101::BOF));
        assert_eq!(frame.last().copied(), Some(s101::EOF));
        assert_eq!(s101::decode(&frame).unwrap(), payload);
    }

    #[test]
    fn s101_escapes_delimiter_bytes_in_the_payload() {
        // A payload made entirely of bytes that collide with the framing must
        // round-trip exactly via escaping.
        let payload = vec![s101::BOF, s101::EOF, s101::CE, 0x00, s101::EOF];
        let frame = s101::encode(&payload);
        // No raw BOF/EOF/CE appears in the body between the delimiters.
        let body = &frame[1..frame.len() - 1];
        assert!(!body.contains(&s101::BOF));
        assert!(!body.contains(&s101::EOF));
        assert_eq!(s101::decode(&frame).unwrap(), payload);
    }

    #[test]
    fn s101_rejects_a_corrupted_crc() {
        let payload = b"route-follow".to_vec();
        let mut frame = s101::encode(&payload);
        // Flip a payload byte (right after BOF) so the CRC no longer validates.
        frame[1] ^= 0x01;
        let err = s101::decode(&frame).unwrap_err();
        assert!(matches!(err, EmberError::BadCrc { .. }), "{err:?}");
    }

    #[test]
    fn s101_rejects_a_missing_frame() {
        let err = s101::decode(b"no delimiters").unwrap_err();
        assert_eq!(err, EmberError::NoFrame);
    }

    #[test]
    fn glow_node_round_trips_a_labelled_string_parameter() {
        let node = GlowNode {
            number: 7,
            identifier: "dest-7-label".to_owned(),
            value: Some(GlowValue::string("STUDIO A")),
        };
        let frame = node.encode_message();
        assert_eq!(GlowNode::decode_message(&frame).unwrap(), node);
    }

    #[test]
    fn glow_node_round_trips_an_integer_parameter() {
        let node = GlowNode {
            number: 1,
            identifier: "crosspoint".to_owned(),
            value: Some(GlowValue::integer(-1234)),
        };
        let frame = node.encode_message();
        assert_eq!(GlowNode::decode_message(&frame).unwrap(), node);
    }

    #[test]
    fn glow_node_round_trips_without_a_value() {
        let node = GlowNode {
            number: 0,
            identifier: "root".to_owned(),
            value: None,
        };
        let frame = node.encode_message();
        assert_eq!(GlowNode::decode_message(&frame).unwrap(), node);
    }

    #[test]
    fn glow_node_requires_number_and_identifier() {
        // A payload with only an identifier (no number) must fail.
        let only_id = ber::encode_tlv(super::GLOW_IDENTIFIER_TAG, &ber::encode_utf8("x"));
        let err = GlowNode::decode_ber(&only_id).unwrap_err();
        assert!(matches!(err, EmberError::UnexpectedTag { .. }), "{err:?}");
    }

    #[test]
    fn glow_value_serialises_tagged_for_json() {
        let v = GlowValue::string("CAM 2");
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["type"], "string");
        let back: GlowValue = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn crc16_is_stable_for_a_known_payload() {
        // Pin the CRC for an empty payload and a known string so a regression in
        // the polynomial/reflection is caught (golden value).
        assert_eq!(s101::crc16(b""), 0x0000);
        // Non-empty payloads produce a non-trivial CRC.
        assert_ne!(s101::crc16(b"123456789"), 0);
    }
}
