//! The RFC 2974 **SAP packet** codec — a pure byte-slice ⇄ typed value layer.
//!
//! One **flags byte** packs six fields `MSB`-first, then an **auth-length** byte
//! (in 32-bit **words**), a 16-bit **message-id hash**, the **originating
//! source** (4 or 16 bytes), optional auth data, an optional NUL-terminated
//! **payload-type** string, and the opaque **SDP** body. This codec is
//! **SDP-content agnostic**: it never inspects the SDP beyond detecting whether
//! the payload-type string was omitted (the body begins `v=0`).
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! | V=1 |A|R|T|E|C|  auth len     |         msg id hash           |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |               originating source (32 or 128 bits)             |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |            optional auth data (auth_len * 4 bytes)            |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! | optional payload type ("application/sdp\0") |     SDP ...     |
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! Correctness rules honoured (ADR-0041 §8, adversarially verified): the
//! **version is the top 3 bits** (`flags >> 5 == 1`); `auth_len` counts 32-bit
//! **words** (`skip auth_len*4` — never VLC's byte-count bug); the message-id
//! **hash is never 0** (rejected on parse, never emitted); **`E=1` is
//! rejected** (no `SAPv2` algorithm); **`C=1` (zlib) is inflated under a hard
//! output cap** (`MAX_SDP_PAYLOAD`), so a compressed announcement can never
//! become a decompression bomb; all arithmetic is checked, nothing panics or
//! indexes out of range.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

use super::SapError;

/// The SAP version this codec accepts and emits (RFC 2974 fixes it at 1, carried
/// in the **top 3 bits** of the flags byte).
pub const SAP_VERSION: u8 = 1;

/// The payload-type MIME string an announcement carries (`application/sdp`).
/// Emitted NUL-terminated; on parse it may be omitted (the body begins `v=0`).
pub const SDP_MIME_TYPE: &str = "application/sdp";

/// The hard upper bound on the opaque SDP payload a parsed packet may carry.
///
/// 64 KiB — larger than any real SDP, small enough that an adversarial
/// announcement can never make the parser allocate unboundedly (brief §9). A
/// datagram claiming more is rejected with [`SapError::PayloadTooLarge`]. It is
/// also the hard output cap the `C=1` zlib inflate path enforces, so a
/// compressed announcement's decompressed SDP is bounded exactly as an
/// uncompressed one ([`SapError::DecompressedTooLarge`] past the cap).
pub const MAX_SDP_PAYLOAD: usize = 64 * 1024;

/// `A` — address-type bit: 0 = IPv4 (32-bit) origin, 1 = IPv6 (128-bit) origin.
const FLAG_ADDR_V6: u8 = 0x10;
/// `T` — message-type bit: 0 = announcement, 1 = deletion.
const FLAG_DELETE: u8 = 0x04;
/// `E` — encryption bit (rejected on parse; never set on encode).
const FLAG_ENCRYPTED: u8 = 0x02;
/// `C` — compression bit (a `C=1` zlib body is inflated under a cap on parse;
/// never set on encode).
const FLAG_COMPRESSED: u8 = 0x01;

/// Byte offset of the 16-bit message-id hash.
const HASH_OFFSET: usize = 2;
/// Byte offset of the originating source.
const ORIGIN_OFFSET: usize = 4;
/// The prefix that marks an SDP body (so an omitted payload-type is detected).
const SDP_BODY_PREFIX: &[u8] = b"v=0";

/// Whether a SAP message announces a session or requests its deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SapMessageType {
    /// An announcement (`T=0`): the payload carries the session's SDP.
    Announcement,
    /// A deletion (`T=1`): a request to withdraw a previously-announced session.
    /// Inbound deletions are a hijack vector and are **ignored** against tracked
    /// sessions (ADR-0041 §8); we emit one only as a courtesy on teardown.
    Deletion,
}

/// A parsed (or to-be-encoded) RFC 2974 SAP packet.
///
/// The [`payload`](SapPacket::payload) is the **opaque** SDP body — this type
/// never parses it. A session is keyed by
/// (`msg_id_hash`, `origin`) (see [`SessionKey`](super::session::SessionKey)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SapPacket {
    /// Announcement (`T=0`) or deletion (`T=1`).
    pub message_type: SapMessageType,
    /// The 16-bit message-id hash. **Never 0** (0 is reserved): the type is
    /// [`NonZeroU16`], so a zero hash cannot be constructed or emitted, and
    /// [`SapPacket::parse`] rejects a wire 0 with [`SapError::ZeroHash`].
    pub msg_id_hash: NonZeroU16,
    /// The originating source address. Its family selects the `A` bit on encode
    /// (IPv4 → `A=0`, 4 bytes; IPv6 → `A=1`, 16 bytes).
    pub origin: IpAddr,
    /// The optional payload-type. `None` when the body begins `v=0` (the type is
    /// omitted on the wire); `Some(mime)` for an explicit NUL-terminated type
    /// (e.g. [`SDP_MIME_TYPE`]).
    pub payload_type: Option<String>,
    /// The opaque SDP body bytes (never parsed by this codec).
    pub payload: Vec<u8>,
}

impl SapPacket {
    /// Parse a SAP packet from `bytes`.
    ///
    /// Validates the version (top 3 bits), rejects encrypted/zero-hash packets,
    /// reads the origin per the `A` bit, **skips `auth_len` × 4** bytes of auth
    /// data, **inflates the body under a hard cap when `C=1`** (RFC 2974 §3
    /// compresses the payload-type + payload together), and splits the
    /// payload-type (omitted when the body begins `v=0`) from the opaque SDP
    /// payload.
    ///
    /// # Errors
    ///
    /// * [`SapError::TooShort`] if the buffer is smaller than the header
    ///   structure (flags, auth-length, hash, origin, `auth_len`×4 auth words)
    ///   declares.
    /// * [`SapError::BadVersion`] if the version field is not [`SAP_VERSION`].
    /// * [`SapError::Encrypted`] if `E=1`.
    /// * [`SapError::DecompressFailed`] if `C=1` and the body is not a valid zlib
    ///   stream; [`SapError::DecompressedTooLarge`] if inflating it would exceed
    ///   [`MAX_SDP_PAYLOAD`] (a decompression-bomb guard).
    /// * [`SapError::ZeroHash`] if the message-id hash is 0 (reserved).
    /// * [`SapError::PayloadTooLarge`] if the SDP body exceeds
    ///   [`MAX_SDP_PAYLOAD`].
    /// * [`SapError::MalformedPayloadType`] if a non-`v=0` body carries no
    ///   NUL-terminated (valid-UTF-8) MIME type.
    pub fn parse(bytes: &[u8]) -> Result<Self, SapError> {
        let flags = *bytes.first().ok_or(SapError::TooShort {
            need: 1,
            got: bytes.len(),
        })?;
        let version = flags >> 5;
        if version != SAP_VERSION {
            return Err(SapError::BadVersion(version));
        }
        if (flags & FLAG_ENCRYPTED) != 0 {
            return Err(SapError::Encrypted);
        }
        let compressed = (flags & FLAG_COMPRESSED) != 0;
        let message_type = if (flags & FLAG_DELETE) != 0 {
            SapMessageType::Deletion
        } else {
            SapMessageType::Announcement
        };
        let address_v6 = (flags & FLAG_ADDR_V6) != 0;

        let auth_len_words = usize::from(*bytes.get(1).ok_or(SapError::TooShort {
            need: 2,
            got: bytes.len(),
        })?);

        let hash_raw = read_u16(bytes, HASH_OFFSET)?;
        let msg_id_hash = NonZeroU16::new(hash_raw).ok_or(SapError::ZeroHash)?;

        let origin_len = if address_v6 { 16usize } else { 4usize };
        let origin_end = ORIGIN_OFFSET
            .checked_add(origin_len)
            .ok_or(SapError::TooShort {
                need: usize::MAX,
                got: bytes.len(),
            })?;
        let origin_slice = bytes
            .get(ORIGIN_OFFSET..origin_end)
            .ok_or(SapError::TooShort {
                need: origin_end,
                got: bytes.len(),
            })?;
        let origin = ip_from_slice(origin_slice, address_v6)?;

        // Skip auth: `auth_len` counts 32-bit WORDS, so `auth_len * 4` bytes
        // (never VLC's `buf += auth_len` byte-count bug).
        let auth_bytes = auth_len_words.checked_mul(4).ok_or(SapError::TooShort {
            need: usize::MAX,
            got: bytes.len(),
        })?;
        let body_start = origin_end
            .checked_add(auth_bytes)
            .ok_or(SapError::TooShort {
                need: usize::MAX,
                got: bytes.len(),
            })?;
        let body = bytes.get(body_start..).ok_or(SapError::TooShort {
            need: body_start,
            got: bytes.len(),
        })?;

        // When `C=1`, the region after the auth header — the payload-type field
        // AND the payload (RFC 2974 §3 compresses them together) — is a zlib
        // stream. Inflate it with a HARD output cap so a decompression bomb (a
        // tiny datagram that expands without bound) cannot exhaust memory: the
        // decompressed body is bounded by the SAME `MAX_SDP_PAYLOAD` an
        // uncompressed announcement is, enforced DURING inflate, never after.
        let inflated;
        let body = if compressed {
            inflated = inflate_zlib_capped(body)?;
            inflated.as_slice()
        } else {
            body
        };

        let (payload_type, payload) = split_payload_type(body)?;
        if payload.len() > MAX_SDP_PAYLOAD {
            return Err(SapError::PayloadTooLarge {
                size: payload.len(),
                max: MAX_SDP_PAYLOAD,
            });
        }

        Ok(Self {
            message_type,
            msg_id_hash,
            origin,
            payload_type,
            payload: payload.to_vec(),
        })
    }

    /// Encode this packet to its RFC 2974 wire form.
    ///
    /// Always emits version 1, `auth_len = 0`, `E=0`, `C=0`, and a non-zero hash
    /// (guaranteed by the [`NonZeroU16`] field). The `A` bit follows the origin
    /// family. An explicit [`payload_type`](SapPacket::payload_type) is emitted
    /// NUL-terminated before the payload; when it is `None` the SDP body follows
    /// the header directly.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut flags = SAP_VERSION << 5; // version in the top 3 bits
        if self.origin.is_ipv6() {
            flags |= FLAG_ADDR_V6;
        }
        if self.message_type == SapMessageType::Deletion {
            flags |= FLAG_DELETE;
        }
        // E / C / R stay 0.
        out.push(flags);
        out.push(0); // auth_len = 0 words
        out.extend_from_slice(&self.msg_id_hash.get().to_be_bytes());
        match self.origin {
            IpAddr::V4(v4) => out.extend_from_slice(&v4.octets()),
            IpAddr::V6(v6) => out.extend_from_slice(&v6.octets()),
        }
        // No auth data (auth_len = 0).
        if let Some(mime) = &self.payload_type {
            out.extend_from_slice(mime.as_bytes());
            out.push(0); // NUL terminator
        }
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Inflate a `C=1` zlib body ([RFC 2974] §3) with a hard output cap.
///
/// The cap ([`MAX_SDP_PAYLOAD`]) is enforced *during* inflate by `miniz_oxide`,
/// so a decompression bomb — a tiny compressed datagram that would expand
/// without bound — can never allocate past the cap; the decompressed body is
/// bounded exactly as an uncompressed one. A stream that would exceed the cap
/// yields [`SapError::DecompressedTooLarge`]; a corrupt or truncated stream
/// yields [`SapError::DecompressFailed`] (SAP is unauthenticated, so a malformed
/// compressed announcement is simply dropped, never a panic).
///
/// [RFC 2974]: https://www.rfc-editor.org/rfc/rfc2974
fn inflate_zlib_capped(compressed: &[u8]) -> Result<Vec<u8>, SapError> {
    use miniz_oxide::inflate::{decompress_to_vec_zlib_with_limit, TINFLStatus};
    decompress_to_vec_zlib_with_limit(compressed, MAX_SDP_PAYLOAD).map_err(|err| {
        if err.status == TINFLStatus::HasMoreOutput {
            SapError::DecompressedTooLarge {
                max: MAX_SDP_PAYLOAD,
            }
        } else {
            SapError::DecompressFailed
        }
    })
}

/// Split the body into its optional payload-type and the opaque payload.
///
/// If the body begins `v=0` the type was omitted (SDP starts immediately);
/// otherwise the leading bytes up to the first NUL are the MIME type.
fn split_payload_type(body: &[u8]) -> Result<(Option<String>, &[u8]), SapError> {
    if body.starts_with(SDP_BODY_PREFIX) {
        return Ok((None, body));
    }
    let nul = body
        .iter()
        .position(|&b| b == 0)
        .ok_or(SapError::MalformedPayloadType)?;
    let mime_bytes = body.get(..nul).ok_or(SapError::MalformedPayloadType)?;
    let payload_start = nul.checked_add(1).ok_or(SapError::MalformedPayloadType)?;
    let payload = body
        .get(payload_start..)
        .ok_or(SapError::MalformedPayloadType)?;
    let mime = core::str::from_utf8(mime_bytes).map_err(|_| SapError::MalformedPayloadType)?;
    Ok((Some(mime.to_owned()), payload))
}

/// Build an [`IpAddr`] from exactly 4 (IPv4) or 16 (IPv6) origin bytes.
fn ip_from_slice(slice: &[u8], v6: bool) -> Result<IpAddr, SapError> {
    if v6 {
        let arr: [u8; 16] = slice.try_into().map_err(|_| SapError::TooShort {
            need: 16,
            got: slice.len(),
        })?;
        Ok(IpAddr::V6(Ipv6Addr::from(arr)))
    } else {
        let arr: [u8; 4] = slice.try_into().map_err(|_| SapError::TooShort {
            need: 4,
            got: slice.len(),
        })?;
        Ok(IpAddr::V4(Ipv4Addr::from(arr)))
    }
}

/// Read a big-endian `u16` at `offset`, or [`SapError::TooShort`].
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, SapError> {
    let end = offset.checked_add(2).ok_or(SapError::TooShort {
        need: usize::MAX,
        got: bytes.len(),
    })?;
    let slice = bytes.get(offset..end).ok_or(SapError::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let hi = *slice.first().ok_or(SapError::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    let lo = *slice.get(1).ok_or(SapError::TooShort {
        need: end,
        got: bytes.len(),
    })?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}
