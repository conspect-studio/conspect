//! A pure, panic-free STUN/TURN message codec (RFC 5389, RFC 5766, RFC 8656).
//!
//! This is the wire-format foundation of the in-crate TURN client
//! ([`super::client`]). It encodes and decodes the subset of STUN/TURN the client
//! needs — Binding, Allocate, Refresh, `CreatePermission`, `ChannelBind`,
//! Send/Data —
//! including the XOR address transform (v4 + v6), MESSAGE-INTEGRITY
//! (HMAC-SHA1 over the message keyed by the long-term credential key),
//! FINGERPRINT (CRC-32 with the `0x5354554e` XOR per RFC 5389 §15.5), and the
//! TURN attributes (REQUESTED-TRANSPORT, LIFETIME, DATA, CHANNEL-NUMBER,
//! XOR-PEER-ADDRESS, REALM, NONCE, ERROR-CODE).
//!
//! It is `forbid(unsafe)`, allocates only the message buffer, and never panics on
//! malformed input — a truncated or nonsensical datagram returns
//! [`TurnError`](crate::TurnError) rather than indexing out of bounds. Because it
//! is socket-free it is exhaustively unit-tested without a network or str0m.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use hmac::{Hmac, KeyInit, Mac};
use md5::{Digest, Md5};
use rand::Rng;
use sha1::Sha1;

use crate::error::TurnError;

/// The STUN magic cookie (RFC 5389 §6): bytes 4..8 of every STUN message.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;

/// The XOR value FINGERPRINT applies to the CRC-32 (RFC 5389 §15.5).
const FINGERPRINT_XOR: u32 = 0x5354_554e;

// STUN method codes (the 12-bit method, RFC 5389 §6 + RFC 5766 §13).
const METHOD_BINDING: u16 = 0x0001;
const METHOD_ALLOCATE: u16 = 0x0003;
const METHOD_REFRESH: u16 = 0x0004;
const METHOD_SEND: u16 = 0x0006;
const METHOD_DATA: u16 = 0x0007;
const METHOD_CREATE_PERMISSION: u16 = 0x0008;
const METHOD_CHANNEL_BIND: u16 = 0x0009;

// Attribute type codes.
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_ADDRESS_FAMILY: u16 = 0x0017;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_FINGERPRINT: u16 = 0x8028;

// Address family bytes inside a (XOR-)MAPPED-ADDRESS value (RFC 5389 §15.1) — the
// same byte values REQUESTED-ADDRESS-FAMILY uses (RFC 8656 §14.7).
const FAMILY_IPV4: u8 = 0x01;
const FAMILY_IPV6: u8 = 0x02;

/// The address family a TURN client requests for its relayed transport address
/// via REQUESTED-ADDRESS-FAMILY (RFC 8656 §14.7). IPv6-first per ADR-0042: the
/// client asks for an IPv6 relay so an IPv6 peer's `CreatePermission` does not
/// fail `440` (Address Family not Supported) / `443` against an IPv4-only default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddressFamily {
    /// IPv4 relay (`0x01`).
    Ipv4,
    /// IPv6 relay (`0x02`).
    Ipv6,
}

impl AddressFamily {
    /// The single family byte (RFC 8656 §14.7: top byte of the 4-byte value).
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Ipv4 => FAMILY_IPV4,
            Self::Ipv6 => FAMILY_IPV6,
        }
    }

    /// Decode a family byte, if recognised.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            FAMILY_IPV4 => Some(Self::Ipv4),
            FAMILY_IPV6 => Some(Self::Ipv6),
            _ => None,
        }
    }
}

/// The IANA protocol number TURN's REQUESTED-TRANSPORT carries for UDP (17),
/// in the top byte of the 4-byte value (RFC 5766 §14.7).
const REQUESTED_TRANSPORT_UDP: u8 = 17;

/// The STUN message class (the two class bits of the message type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Class {
    /// A request (expects a success/error response).
    Request,
    /// An indication (no response expected; TURN Send/Data ride this).
    Indication,
    /// A success response.
    Success,
    /// An error response.
    Error,
}

impl Class {
    /// The two class bits as positioned in the 14-bit message type.
    const fn bits(self) -> u16 {
        // Class is encoded in bits 4 and 8 of the message type (RFC 5389 §6).
        match self {
            Self::Request => 0x0000,
            Self::Indication => 0x0010,
            Self::Success => 0x0100,
            Self::Error => 0x0110,
        }
    }

    const fn from_type(message_type: u16) -> Self {
        match message_type & 0x0110 {
            0x0000 => Self::Request,
            0x0010 => Self::Indication,
            0x0100 => Self::Success,
            _ => Self::Error,
        }
    }
}

/// The STUN/TURN method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// STUN Binding (server-reflexive discovery / connectivity checks).
    Binding,
    /// TURN Allocate.
    Allocate,
    /// TURN Refresh.
    Refresh,
    /// TURN Send indication.
    Send,
    /// TURN Data indication.
    Data,
    /// TURN `CreatePermission`.
    CreatePermission,
    /// TURN `ChannelBind`.
    ChannelBind,
}

impl Method {
    const fn code(self) -> u16 {
        match self {
            Self::Binding => METHOD_BINDING,
            Self::Allocate => METHOD_ALLOCATE,
            Self::Refresh => METHOD_REFRESH,
            Self::Send => METHOD_SEND,
            Self::Data => METHOD_DATA,
            Self::CreatePermission => METHOD_CREATE_PERMISSION,
            Self::ChannelBind => METHOD_CHANNEL_BIND,
        }
    }

    const fn from_code(code: u16) -> Option<Self> {
        match code {
            METHOD_BINDING => Some(Self::Binding),
            METHOD_ALLOCATE => Some(Self::Allocate),
            METHOD_REFRESH => Some(Self::Refresh),
            METHOD_SEND => Some(Self::Send),
            METHOD_DATA => Some(Self::Data),
            METHOD_CREATE_PERMISSION => Some(Self::CreatePermission),
            METHOD_CHANNEL_BIND => Some(Self::ChannelBind),
            _ => None,
        }
    }
}

/// A 96-bit STUN transaction id (RFC 5389 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId([u8; 12]);

impl TransactionId {
    /// Mint a fresh random transaction id from the OS RNG.
    #[must_use]
    pub fn random() -> Self {
        let mut bytes = [0u8; 12];
        rand::rng().fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// The raw 12 transaction-id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 12] {
        &self.0
    }
}

/// One STUN/TURN attribute the codec understands.
///
/// Unknown comprehension-optional attributes are dropped on parse; unknown
/// comprehension-required attributes (type < 0x8000) surface as
/// [`Attribute::Unknown`] so the client can react (e.g. log) without failing the
/// whole message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Attribute {
    /// MAPPED-ADDRESS (non-XOR; legacy, parsed for completeness).
    MappedAddress(SocketAddr),
    /// XOR-MAPPED-ADDRESS — the server-reflexive address from a Binding/Allocate
    /// success.
    XorMappedAddress(SocketAddr),
    /// XOR-RELAYED-ADDRESS — the relay transport address from an Allocate success.
    XorRelayedAddress(SocketAddr),
    /// XOR-PEER-ADDRESS — the peer of a Send/Data indication or a permission.
    XorPeerAddress(SocketAddr),
    /// USERNAME (long-term credential).
    Username(String),
    /// REALM (long-term credential).
    Realm(String),
    /// NONCE (long-term credential).
    Nonce(String),
    /// LIFETIME, in seconds.
    Lifetime(u32),
    /// `REQUESTED-TRANSPORT` = UDP (the only transport the client requests).
    RequestedTransportUdp,
    /// `REQUESTED-ADDRESS-FAMILY` — the relay family the client asks for on
    /// Allocate (RFC 8656 §14.7; IPv6-first per ADR-0042).
    RequestedAddressFamily(AddressFamily),
    /// DATA — an application payload in a Send/Data indication.
    Data(Vec<u8>),
    /// CHANNEL-NUMBER for a `ChannelBind`.
    ChannelNumber(u16),
    /// ERROR-CODE (class*100 + number) plus its reason phrase.
    ErrorCode {
        /// The numeric STUN error code (e.g. 401, 438).
        code: u16,
        /// The reason phrase.
        reason: String,
    },
    /// An unknown comprehension-required attribute (type < 0x8000) the codec did
    /// not model — kept so the client can detect it.
    Unknown(u16),
}

/// A decoded STUN/TURN message, or one being built for transmission.
#[derive(Debug, Clone)]
pub struct StunMessage {
    class: Class,
    method: Method,
    transaction_id: TransactionId,
    attributes: Vec<Attribute>,
    /// On a parsed message, the raw bytes — needed to re-verify MESSAGE-INTEGRITY
    /// and FINGERPRINT, which are computed over the exact wire bytes.
    raw: Option<Vec<u8>>,
}

impl StunMessage {
    fn new(class: Class, method: Method) -> Self {
        Self {
            class,
            method,
            transaction_id: TransactionId::random(),
            attributes: Vec::new(),
            raw: None,
        }
    }

    /// A request of `method` with a fresh transaction id.
    #[must_use]
    pub fn request(method: Method) -> Self {
        Self::new(Class::Request, method)
    }

    /// An indication of `method` (Send/Data) with a fresh transaction id.
    #[must_use]
    pub fn indication(method: Method) -> Self {
        Self::new(Class::Indication, method)
    }

    /// A success response of `method` (used by tests / a fake server).
    #[must_use]
    pub fn success(method: Method) -> Self {
        Self::new(Class::Success, method)
    }

    /// An error response of `method` (used by tests / a fake server).
    #[must_use]
    pub fn error(method: Method) -> Self {
        Self::new(Class::Error, method)
    }

    /// Build a request reusing an existing transaction id (e.g. an auth retry on
    /// the same logical request must NOT reuse the id; this is for responses).
    #[must_use]
    pub fn with_transaction(class: Class, method: Method, transaction_id: TransactionId) -> Self {
        Self {
            class,
            method,
            transaction_id,
            attributes: Vec::new(),
            raw: None,
        }
    }

    /// Append an attribute (builder-style, by value to keep call sites terse).
    pub fn push(&mut self, attr: Attribute) {
        self.attributes.push(attr);
    }

    /// The message class.
    #[must_use]
    pub const fn class(&self) -> Class {
        self.class
    }

    /// The message method.
    #[must_use]
    pub const fn method(&self) -> Method {
        self.method
    }

    /// The transaction id.
    #[must_use]
    pub const fn transaction_id(&self) -> TransactionId {
        self.transaction_id
    }

    /// The decoded attributes.
    #[must_use]
    pub fn attributes(&self) -> &[Attribute] {
        &self.attributes
    }

    /// The XOR-MAPPED (or legacy MAPPED) address, if present.
    #[must_use]
    pub fn mapped_address(&self) -> Option<SocketAddr> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::XorMappedAddress(addr) | Attribute::MappedAddress(addr) => Some(*addr),
            _ => None,
        })
    }

    /// The XOR-RELAYED-ADDRESS (the allocated relay), if present.
    #[must_use]
    pub fn relayed_address(&self) -> Option<SocketAddr> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::XorRelayedAddress(addr) => Some(*addr),
            _ => None,
        })
    }

    /// The XOR-PEER-ADDRESS, if present.
    #[must_use]
    pub fn peer_address(&self) -> Option<SocketAddr> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::XorPeerAddress(addr) => Some(*addr),
            _ => None,
        })
    }

    /// The REQUESTED-ADDRESS-FAMILY, if present.
    #[must_use]
    pub fn requested_address_family(&self) -> Option<AddressFamily> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::RequestedAddressFamily(family) => Some(*family),
            _ => None,
        })
    }

    /// The LIFETIME in seconds, if present.
    #[must_use]
    pub fn lifetime(&self) -> Option<u32> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Lifetime(v) => Some(*v),
            _ => None,
        })
    }

    /// The DATA payload, if present.
    #[must_use]
    pub fn data(&self) -> Option<&[u8]> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Data(d) => Some(d.as_slice()),
            _ => None,
        })
    }

    /// The REALM, if present.
    #[must_use]
    pub fn realm(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Realm(r) => Some(r.as_str()),
            _ => None,
        })
    }

    /// The NONCE, if present.
    #[must_use]
    pub fn nonce(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::Nonce(n) => Some(n.as_str()),
            _ => None,
        })
    }

    /// The numeric ERROR-CODE, if present.
    #[must_use]
    pub fn error_code(&self) -> Option<u16> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::ErrorCode { code, .. } => Some(*code),
            _ => None,
        })
    }

    /// The ERROR-CODE reason phrase, if present.
    #[must_use]
    pub fn error_reason(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a {
            Attribute::ErrorCode { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
    }

    /// Serialize the message to its wire bytes.
    ///
    /// If `integrity_key` is `Some`, a MESSAGE-INTEGRITY attribute is computed
    /// over the message (HMAC-SHA1 keyed by that long-term key) and appended
    /// before the FINGERPRINT. A FINGERPRINT (CRC-32) is always appended last.
    #[must_use]
    pub fn to_bytes(&self, integrity_key: Option<&[u8]>) -> Vec<u8> {
        let mut body = Vec::with_capacity(64);
        for attr in &self.attributes {
            encode_attribute(&mut body, attr, &self.transaction_id);
        }

        // Reserve space for MESSAGE-INTEGRITY (4 header + 20 hmac) and FINGERPRINT
        // (4 header + 4 crc) so the length field is correct when we compute them.
        let integrity_len = if integrity_key.is_some() { 24 } else { 0 };
        let fingerprint_len = 8usize;

        let mut out = Vec::with_capacity(20 + body.len() + integrity_len + fingerprint_len);
        let message_type = self.method.code() | self.class.bits();
        out.extend_from_slice(&message_type.to_be_bytes());

        // Length BEFORE message-integrity, but including its 24 bytes (RFC 5389
        // §15.4 computes the HMAC over the header with the length field already
        // covering everything up to and including MESSAGE-INTEGRITY).
        let len_through_integrity = body.len() + integrity_len;
        push_len(&mut out, len_through_integrity);
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(self.transaction_id.as_bytes());
        out.extend_from_slice(&body);

        if let Some(key) = integrity_key {
            let mac = hmac_sha1(key, &out);
            out.extend_from_slice(&ATTR_MESSAGE_INTEGRITY.to_be_bytes());
            out.extend_from_slice(&20u16.to_be_bytes());
            out.extend_from_slice(&mac);
        }

        // FINGERPRINT: rewrite the length to cover the fingerprint attribute, CRC
        // the message so far, XOR per RFC 5389 §15.5.
        let len_through_fingerprint = len_through_integrity + fingerprint_len;
        let len_bytes = u16::try_from(len_through_fingerprint)
            .unwrap_or(u16::MAX)
            .to_be_bytes();
        if let Some(slot) = out.get_mut(2..4) {
            slot.copy_from_slice(&len_bytes);
        }
        let crc = crc32(&out) ^ FINGERPRINT_XOR;
        out.extend_from_slice(&ATTR_FINGERPRINT.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&crc.to_be_bytes());

        out
    }

    /// Parse a STUN/TURN message from a datagram.
    ///
    /// # Errors
    ///
    /// [`TurnError::NotStun`] if the first two bits are non-zero or the magic
    /// cookie is wrong; [`TurnError::Malformed`] for a truncated header/attribute
    /// or a bad length.
    pub fn parse(buf: &[u8]) -> Result<Self, TurnError> {
        if buf.len() < 20 {
            return Err(TurnError::Malformed("message shorter than 20-byte header"));
        }
        let b0 = *buf.first().ok_or(TurnError::Malformed("empty"))?;
        // RFC 5389: the two most-significant bits of a STUN message are zero.
        if b0 & 0xC0 != 0 {
            return Err(TurnError::NotStun);
        }
        let message_type = be16(buf, 0)?;
        let length = usize::from(be16(buf, 2)?);
        let cookie = be32(buf, 4)?;
        if cookie != MAGIC_COOKIE {
            return Err(TurnError::NotStun);
        }
        let mut tid = [0u8; 12];
        tid.copy_from_slice(buf.get(8..20).ok_or(TurnError::Malformed("short header"))?);
        let transaction_id = TransactionId(tid);

        let method_code = decode_method_bits(message_type);
        let method =
            Method::from_code(method_code).ok_or(TurnError::Malformed("unknown STUN method"))?;
        let class = Class::from_type(message_type);

        let body_end = 20usize
            .checked_add(length)
            .ok_or(TurnError::Malformed("length overflow"))?;
        if body_end > buf.len() {
            return Err(TurnError::Malformed("declared length exceeds datagram"));
        }
        let body = buf.get(20..body_end).ok_or(TurnError::Malformed("body"))?;

        let mut attributes = Vec::new();
        let mut offset = 0usize;
        while offset + 4 <= body.len() {
            let attr_type = be16(body, offset)?;
            let attr_len = usize::from(be16(body, offset + 2)?);
            let value_start = offset + 4;
            let value_end = value_start
                .checked_add(attr_len)
                .ok_or(TurnError::Malformed("attr length overflow"))?;
            if value_end > body.len() {
                return Err(TurnError::Malformed("attribute exceeds body"));
            }
            let value = body
                .get(value_start..value_end)
                .ok_or(TurnError::Malformed("attr value"))?;
            if let Some(attr) = decode_attribute(attr_type, value, &transaction_id)? {
                attributes.push(attr);
            }
            // Attributes are padded to a 4-byte boundary.
            let padded = (attr_len + 3) & !3;
            offset = value_start
                .checked_add(padded)
                .ok_or(TurnError::Malformed("attr padding overflow"))?;
        }

        let raw = buf
            .get(..body_end)
            .ok_or(TurnError::Malformed("body slice"))?
            .to_vec();
        Ok(Self {
            class,
            method,
            transaction_id,
            attributes,
            raw: Some(raw),
        })
    }

    /// Verify the MESSAGE-INTEGRITY attribute against `key` (the long-term key).
    ///
    /// Returns `false` if the message has no integrity attribute, was not parsed
    /// from raw bytes, or the HMAC does not match.
    #[must_use]
    pub fn verify_integrity(&self, key: &[u8]) -> bool {
        let Some(raw) = self.raw.as_deref() else {
            return false;
        };
        // Find the MESSAGE-INTEGRITY attribute in the raw body and recompute the
        // HMAC over the message up to (but not including) it, with the length
        // field temporarily set to cover through MESSAGE-INTEGRITY.
        let Some((mi_offset, mi_value)) = find_attribute(raw, ATTR_MESSAGE_INTEGRITY) else {
            return false;
        };
        if mi_value.len() != 20 {
            return false;
        }
        // Bytes up to the MESSAGE-INTEGRITY attribute header.
        let Some(prefix) = raw.get(..mi_offset) else {
            return false;
        };
        let mut hashed = prefix.to_vec();
        // The length used in the HMAC covers everything through MESSAGE-INTEGRITY.
        let len_through_mi = (mi_offset + 4 + 20).saturating_sub(20);
        let len_bytes = u16::try_from(len_through_mi)
            .unwrap_or(u16::MAX)
            .to_be_bytes();
        if let Some(slot) = hashed.get_mut(2..4) {
            slot.copy_from_slice(&len_bytes);
        }
        let computed = hmac_sha1(key, &hashed);
        constant_time_eq(&computed, mi_value)
    }

    /// Validate the FINGERPRINT attribute (CRC-32) over the raw bytes.
    #[must_use]
    pub fn has_valid_fingerprint(&self) -> bool {
        let Some(raw) = self.raw.as_deref() else {
            return false;
        };
        let Some((fp_offset, fp_value)) = find_attribute(raw, ATTR_FINGERPRINT) else {
            return false;
        };
        if fp_value.len() != 4 {
            return false;
        }
        let Some(prefix) = raw.get(..fp_offset) else {
            return false;
        };
        let expected = crc32(prefix) ^ FINGERPRINT_XOR;
        let Ok(actual_bytes) = <[u8; 4]>::try_from(fp_value) else {
            return false;
        };
        let actual = u32::from_be_bytes(actual_bytes);
        expected == actual
    }
}

/// Whether `buf` is a TURN `ChannelData` frame (RFC 5766 §11.4): the first two
/// bytes (the channel number) fall in `0x4000..=0x7FFF`, distinguishing it from a
/// STUN message (whose first two bits are zero, so its type is `< 0x4000`).
#[must_use]
pub fn is_channel_data(buf: &[u8]) -> bool {
    match (buf.first(), buf.get(1)) {
        (Some(&hi), Some(&lo)) => {
            let channel = (u16::from(hi) << 8) | u16::from(lo);
            (0x4000..=0x7FFF).contains(&channel)
        }
        _ => false,
    }
}

/// Derive the long-term credential key (RFC 5389 §15.4): `MD5(username ":"
/// realm ":" password)`.
#[must_use]
pub fn long_term_key(username: &str, realm: &str, password: &str) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(username.as_bytes());
    hasher.update(b":");
    hasher.update(realm.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    hasher.finalize().to_vec()
}

// ── private helpers ──────────────────────────────────────────────────────────

fn push_len(out: &mut Vec<u8>, len: usize) {
    let len = u16::try_from(len).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
}

fn be16(buf: &[u8], offset: usize) -> Result<u16, TurnError> {
    let hi = *buf
        .get(offset)
        .ok_or(TurnError::Malformed("truncated u16"))?;
    let lo = *buf
        .get(offset + 1)
        .ok_or(TurnError::Malformed("truncated u16"))?;
    Ok((u16::from(hi) << 8) | u16::from(lo))
}

fn be32(buf: &[u8], offset: usize) -> Result<u32, TurnError> {
    let end = offset
        .checked_add(4)
        .ok_or(TurnError::Malformed("u32 offset overflow"))?;
    let slice = buf
        .get(offset..end)
        .ok_or(TurnError::Malformed("truncated u32"))?;
    let bytes = <[u8; 4]>::try_from(slice).map_err(|_e| TurnError::Malformed("u32 length"))?;
    Ok(u32::from_be_bytes(bytes))
}

/// Recover the 12-bit method from the 14-bit message type (RFC 5389 §6): the
/// method bits are interleaved around the class bits (4 and 8).
const fn decode_method_bits(message_type: u16) -> u16 {
    let m0 = message_type & 0x000F;
    let m1 = (message_type & 0x00E0) >> 1;
    let m2 = (message_type & 0x3E00) >> 2;
    m0 | m1 | m2
}

fn encode_attribute(out: &mut Vec<u8>, attr: &Attribute, tid: &TransactionId) {
    match attr {
        Attribute::MappedAddress(addr) => push_addr(out, ATTR_MAPPED_ADDRESS, *addr, None),
        Attribute::XorMappedAddress(addr) => {
            push_addr(out, ATTR_XOR_MAPPED_ADDRESS, *addr, Some(tid));
        }
        Attribute::XorRelayedAddress(addr) => {
            push_addr(out, ATTR_XOR_RELAYED_ADDRESS, *addr, Some(tid));
        }
        Attribute::XorPeerAddress(addr) => {
            push_addr(out, ATTR_XOR_PEER_ADDRESS, *addr, Some(tid));
        }
        Attribute::Username(s) => push_bytes(out, ATTR_USERNAME, s.as_bytes()),
        Attribute::Realm(s) => push_bytes(out, ATTR_REALM, s.as_bytes()),
        Attribute::Nonce(s) => push_bytes(out, ATTR_NONCE, s.as_bytes()),
        Attribute::Lifetime(v) => push_bytes(out, ATTR_LIFETIME, &v.to_be_bytes()),
        Attribute::RequestedTransportUdp => {
            push_bytes(
                out,
                ATTR_REQUESTED_TRANSPORT,
                &[REQUESTED_TRANSPORT_UDP, 0, 0, 0],
            );
        }
        Attribute::RequestedAddressFamily(family) => {
            // RFC 8656 §14.7: a 4-byte value — the family byte then 3 reserved.
            push_bytes(
                out,
                ATTR_REQUESTED_ADDRESS_FAMILY,
                &[family.byte(), 0, 0, 0],
            );
        }
        Attribute::Data(d) => push_bytes(out, ATTR_DATA, d),
        Attribute::ChannelNumber(n) => {
            let [hi, lo] = n.to_be_bytes();
            push_bytes(out, ATTR_CHANNEL_NUMBER, &[hi, lo, 0, 0]);
        }
        Attribute::ErrorCode { code, reason } => {
            // ERROR-CODE class is the hundreds digit (3..=6), number the rest;
            // both fit a u8 by construction (code < 700).
            let class = u8::try_from(code / 100).unwrap_or(0) & 0x07;
            let number = u8::try_from(code % 100).unwrap_or(0);
            let mut value = vec![0u8, 0u8, class, number];
            value.extend_from_slice(reason.as_bytes());
            push_bytes(out, ATTR_ERROR_CODE, &value);
        }
        Attribute::Unknown(_) => {}
    }
}

fn push_bytes(out: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    out.extend_from_slice(&attr_type.to_be_bytes());
    let len = u16::try_from(value.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    // Pad to a 4-byte boundary.
    let pad = (4 - (value.len() % 4)) % 4;
    out.extend(std::iter::repeat_n(0u8, pad));
}

fn push_addr(out: &mut Vec<u8>, attr_type: u16, addr: SocketAddr, xor: Option<&TransactionId>) {
    let mut value = Vec::with_capacity(20);
    value.push(0); // reserved
    match addr {
        SocketAddr::V4(v4) => {
            value.push(FAMILY_IPV4);
            let port = maybe_xor_port(v4.port(), xor);
            value.extend_from_slice(&port.to_be_bytes());
            let octets = maybe_xor_v4(v4.ip().octets(), xor);
            value.extend_from_slice(&octets);
        }
        SocketAddr::V6(v6) => {
            value.push(FAMILY_IPV6);
            let port = maybe_xor_port(v6.port(), xor);
            value.extend_from_slice(&port.to_be_bytes());
            let octets = maybe_xor_v6(v6.ip().octets(), xor);
            value.extend_from_slice(&octets);
        }
    }
    push_bytes(out, attr_type, &value);
}

fn maybe_xor_port(port: u16, xor: Option<&TransactionId>) -> u16 {
    if xor.is_some() {
        // Port is XORed with the top 16 bits of the magic cookie.
        let cookie = MAGIC_COOKIE.to_be_bytes();
        let top = u16::from_be_bytes([cookie[0], cookie[1]]);
        port ^ top
    } else {
        port
    }
}

fn maybe_xor_v4(octets: [u8; 4], xor: Option<&TransactionId>) -> [u8; 4] {
    if xor.is_none() {
        return octets;
    }
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let mut out = octets;
    for (o, c) in out.iter_mut().zip(cookie.iter()) {
        *o ^= *c;
    }
    out
}

fn maybe_xor_v6(octets: [u8; 16], xor: Option<&TransactionId>) -> [u8; 16] {
    let Some(tid) = xor else {
        return octets;
    };
    // XOR with the magic cookie (4 bytes) followed by the transaction id (12).
    let mut mask = [0u8; 16];
    mask[..4].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    mask[4..].copy_from_slice(tid.as_bytes());
    let mut out = octets;
    for (o, m) in out.iter_mut().zip(mask.iter()) {
        *o ^= *m;
    }
    out
}

fn decode_attribute(
    attr_type: u16,
    value: &[u8],
    tid: &TransactionId,
) -> Result<Option<Attribute>, TurnError> {
    let attr = match attr_type {
        ATTR_MAPPED_ADDRESS => Some(Attribute::MappedAddress(decode_addr(value, None)?)),
        ATTR_XOR_MAPPED_ADDRESS => {
            Some(Attribute::XorMappedAddress(decode_addr(value, Some(tid))?))
        }
        ATTR_XOR_RELAYED_ADDRESS => {
            Some(Attribute::XorRelayedAddress(decode_addr(value, Some(tid))?))
        }
        ATTR_XOR_PEER_ADDRESS => Some(Attribute::XorPeerAddress(decode_addr(value, Some(tid))?)),
        ATTR_USERNAME => Some(Attribute::Username(decode_str(value))),
        ATTR_REALM => Some(Attribute::Realm(decode_str(value))),
        ATTR_NONCE => Some(Attribute::Nonce(decode_str(value))),
        ATTR_LIFETIME => Some(Attribute::Lifetime(be32(value, 0)?)),
        ATTR_REQUESTED_TRANSPORT => Some(Attribute::RequestedTransportUdp),
        ATTR_REQUESTED_ADDRESS_FAMILY => {
            let byte = *value
                .first()
                .ok_or(TurnError::Malformed("address-family"))?;
            AddressFamily::from_byte(byte).map(Attribute::RequestedAddressFamily)
        }
        ATTR_DATA => Some(Attribute::Data(value.to_vec())),
        ATTR_CHANNEL_NUMBER => Some(Attribute::ChannelNumber(be16(value, 0)?)),
        ATTR_ERROR_CODE => {
            let class = u16::from(*value.get(2).ok_or(TurnError::Malformed("error-code"))? & 0x07);
            let number = u16::from(*value.get(3).ok_or(TurnError::Malformed("error-code"))?);
            let reason = decode_str(value.get(4..).unwrap_or(&[]));
            Some(Attribute::ErrorCode {
                code: class * 100 + number,
                reason,
            })
        }
        // MESSAGE-INTEGRITY / FINGERPRINT are verified from raw bytes, not kept.
        ATTR_MESSAGE_INTEGRITY | ATTR_FINGERPRINT => None,
        // Comprehension-required unknowns (type < 0x8000) are surfaced; optional
        // ones (>= 0x8000) are silently ignored.
        other if other < 0x8000 => Some(Attribute::Unknown(other)),
        _ => None,
    };
    Ok(attr)
}

fn decode_str(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn decode_addr(value: &[u8], xor: Option<&TransactionId>) -> Result<SocketAddr, TurnError> {
    let family = *value.get(1).ok_or(TurnError::Malformed("addr family"))?;
    let port = be16(value, 2)?;
    let port = maybe_xor_port(port, xor);
    match family {
        FAMILY_IPV4 => {
            let raw: [u8; 4] = value
                .get(4..8)
                .ok_or(TurnError::Malformed("ipv4 addr"))?
                .try_into()
                .map_err(|_e| TurnError::Malformed("ipv4 addr length"))?;
            let octets = maybe_xor_v4(raw, xor);
            Ok(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::from(octets),
                port,
            )))
        }
        FAMILY_IPV6 => {
            let raw: [u8; 16] = value
                .get(4..20)
                .ok_or(TurnError::Malformed("ipv6 addr"))?
                .try_into()
                .map_err(|_e| TurnError::Malformed("ipv6 addr length"))?;
            let octets = maybe_xor_v6(raw, xor);
            Ok(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(octets),
                port,
                0,
                0,
            )))
        }
        _ => Err(TurnError::Malformed("unknown address family")),
    }
}

/// Find an attribute of `attr_type` in raw bytes, returning `(offset_of_header,
/// value_slice)`. Walks the body the same way `parse` does.
fn find_attribute(raw: &[u8], attr_type: u16) -> Option<(usize, &[u8])> {
    let length = usize::from(u16::from_be_bytes([*raw.get(2)?, *raw.get(3)?]));
    let body_end = 20usize.checked_add(length)?;
    // The recorded length may cover MESSAGE-INTEGRITY/FINGERPRINT or not; scan to
    // the end of the buffer to be safe (attributes are self-delimiting).
    let scan_end = body_end.min(raw.len()).max(20);
    let mut offset = 20usize;
    while offset + 4 <= raw.len() {
        let t = u16::from_be_bytes([*raw.get(offset)?, *raw.get(offset + 1)?]);
        let len = usize::from(u16::from_be_bytes([
            *raw.get(offset + 2)?,
            *raw.get(offset + 3)?,
        ]));
        let value_start = offset + 4;
        let value_end = value_start.checked_add(len)?;
        if value_end > raw.len() {
            return None;
        }
        if t == attr_type {
            return Some((offset, raw.get(value_start..value_end)?));
        }
        let padded = (len + 3) & !3;
        offset = value_start.checked_add(padded)?;
        if offset > scan_end && offset > raw.len() {
            break;
        }
    }
    None
}

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = match HmacSha1::new_from_slice(key) {
        Ok(m) => m,
        // HMAC accepts any key length; this is unreachable in practice.
        Err(_e) => return [0u8; 20],
    };
    mac.update(data);
    let result = mac.finalize().into_bytes();
    let mut out = [0u8; 20];
    out.copy_from_slice(&result);
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// CRC-32 (IEEE 802.3, the polynomial STUN FINGERPRINT uses), computed without a
/// table — the messages are small, and a tableless implementation keeps the codec
/// dependency-free and `forbid(unsafe)`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
