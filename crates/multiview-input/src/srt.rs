//! SRT (Secure Reliable Transport) connection model and option parsing (pure).
//!
//! SRT is a UDP-based contribution transport (the de-facto open protocol).
//! Multiview ingests SRT via libav's `srt://` demuxer (the actual socket and the
//! `libsrt` link live behind the `ffmpeg` feature), but the **connection model**
//! — the call mode, the AES key length, the passphrase, the stream id, latency,
//! and the libav URL/option assembly — is pure and exhaustively testable here.
//!
//! This module owns:
//!
//! * [`SrtMode`] — caller / listener / rendezvous handshake side.
//! * [`KeyLength`] — the AES key length (none / 128 / 192 / 256) used with a
//!   passphrase for the SRT encrypted handshake.
//! * [`StreamId`] — the SRT Access Control stream id (the `#!::r=…,m=…`
//!   convention from SRT Access Control), validated to the protocol's 512-byte
//!   cap.
//! * [`SrtConfig`] — the full connection record, with [`SrtConfig::to_url`]
//!   producing the libav `srt://host:port?...` URL the demuxer opens.
//!
//! No sockets here; the transport is opened by the libav adapter when the
//! `ffmpeg` feature is on. The ingested stream feeds the last-good stores like
//! every other source (invariant #2) and is *sampled*, never pacing.

/// Errors raised while building or validating an SRT connection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SrtError {
    /// A passphrase was outside SRT's valid length (10..=79 characters) while
    /// encryption was requested.
    #[error("srt passphrase length {0} is outside the valid range 10..=79")]
    PassphraseLength(usize),

    /// Encryption (a non-zero key length) was requested without a passphrase, or
    /// a passphrase was supplied with [`KeyLength::None`].
    #[error("srt encryption configuration inconsistent: {0}")]
    Encryption(&'static str),

    /// A stream id exceeded SRT's 512-byte limit.
    #[error("srt stream id is {0} bytes, exceeding the 512-byte limit")]
    StreamIdTooLong(usize),

    /// A required connection parameter (host/port) was missing or invalid.
    #[error("srt connection parameter invalid: {0}")]
    Parameter(&'static str),
}

/// The SRT call mode (which side initiates the handshake).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum SrtMode {
    /// Caller: actively connects to a listener at a known address.
    #[default]
    Caller,
    /// Listener: binds and waits for an incoming caller.
    Listener,
    /// Rendezvous: both peers connect simultaneously (NAT traversal).
    Rendezvous,
}

impl SrtMode {
    /// The libav `mode=` option token for this mode.
    #[must_use]
    pub const fn as_libav_token(self) -> &'static str {
        match self {
            Self::Caller => "caller",
            Self::Listener => "listener",
            Self::Rendezvous => "rendezvous",
        }
    }

    /// Parse a libav `mode=` token into a [`SrtMode`].
    ///
    /// # Errors
    ///
    /// [`SrtError::Parameter`] when the token is not a recognised mode.
    pub fn from_token(token: &str) -> Result<Self, SrtError> {
        match token {
            "caller" => Ok(Self::Caller),
            "listener" => Ok(Self::Listener),
            "rendezvous" => Ok(Self::Rendezvous),
            _ => Err(SrtError::Parameter("unknown srt mode token")),
        }
    }
}

/// The AES key length used for the SRT encrypted handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum KeyLength {
    /// No encryption.
    #[default]
    None,
    /// AES-128.
    Aes128,
    /// AES-192.
    Aes192,
    /// AES-256.
    Aes256,
}

impl KeyLength {
    /// The key length in bytes (`0`, `16`, `24`, `32`) — the SRT `pbkeylen`.
    #[must_use]
    pub const fn bytes(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Aes128 => 16,
            Self::Aes192 => 24,
            Self::Aes256 => 32,
        }
    }

    /// Decode an SRT `pbkeylen` byte count into a [`KeyLength`].
    ///
    /// # Errors
    ///
    /// [`SrtError::Encryption`] when the byte count is not one of `0/16/24/32`.
    pub const fn from_bytes(bytes: u8) -> Result<Self, SrtError> {
        match bytes {
            0 => Ok(Self::None),
            16 => Ok(Self::Aes128),
            24 => Ok(Self::Aes192),
            32 => Ok(Self::Aes256),
            _ => Err(SrtError::Encryption("pbkeylen must be 0, 16, 24, or 32")),
        }
    }

    /// Whether this key length enables encryption.
    #[must_use]
    pub const fn is_encrypted(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// A validated SRT Access Control stream id.
///
/// The stream id is an arbitrary application string (commonly the SRT-AC
/// `#!::r=resource,m=request` convention) capped at 512 bytes by the protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamId(String);

impl StreamId {
    /// SRT's maximum stream-id length in bytes.
    pub const MAX_BYTES: usize = 512;

    /// Validate and wrap a stream id.
    ///
    /// # Errors
    ///
    /// [`SrtError::StreamIdTooLong`] when the id exceeds [`StreamId::MAX_BYTES`].
    pub fn new(value: impl Into<String>) -> Result<Self, SrtError> {
        let value = value.into();
        if value.len() > Self::MAX_BYTES {
            return Err(SrtError::StreamIdTooLong(value.len()));
        }
        Ok(Self(value))
    }

    /// The stream id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A complete SRT connection configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtConfig {
    /// The call mode.
    pub mode: SrtMode,
    /// The host (a listener for caller mode; the bind address for listener mode;
    /// the peer for rendezvous).
    pub host: String,
    /// The UDP port.
    pub port: u16,
    /// The AES key length (encryption is on when this is not [`KeyLength::None`]).
    pub key_length: KeyLength,
    /// The passphrase (required when `key_length` is encrypted).
    pub passphrase: Option<String>,
    /// The SRT Access Control stream id, if any.
    pub stream_id: Option<StreamId>,
    /// The receive latency in milliseconds (SRT `latency`/`rcvlatency`).
    pub latency_ms: u32,
}

impl Default for SrtConfig {
    fn default() -> Self {
        Self {
            mode: SrtMode::Caller,
            host: String::new(),
            port: 0,
            key_length: KeyLength::None,
            passphrase: None,
            stream_id: None,
            latency_ms: 120,
        }
    }
}

impl SrtConfig {
    /// Validate the configuration's internal consistency.
    ///
    /// # Errors
    ///
    /// * [`SrtError::Parameter`] when the host/port are missing where required.
    /// * [`SrtError::Encryption`] / [`SrtError::PassphraseLength`] when the
    ///   passphrase and key length are inconsistent.
    pub fn validate(&self) -> Result<(), SrtError> {
        if self.host.is_empty() {
            return Err(SrtError::Parameter("host is empty"));
        }
        if self.port == 0 {
            return Err(SrtError::Parameter("port is zero"));
        }
        match (self.key_length.is_encrypted(), self.passphrase.as_deref()) {
            (true, None) => {
                return Err(SrtError::Encryption(
                    "encryption requested but no passphrase supplied",
                ))
            }
            (false, Some(_)) => {
                return Err(SrtError::Encryption(
                    "passphrase supplied but key length is None",
                ))
            }
            (true, Some(pp)) => {
                // SRT mandates 10..=79 character passphrases.
                if pp.len() < 10 || pp.len() > 79 {
                    return Err(SrtError::PassphraseLength(pp.len()));
                }
            }
            (false, None) => {}
        }
        Ok(())
    }

    /// Build the libav `srt://host:port?...` URL the demuxer opens.
    ///
    /// Validates first, then assembles the query options (`mode`, `latency`,
    /// `pbkeylen`, `passphrase`, `streamid`). The passphrase is included verbatim
    /// because the libav demuxer requires it; callers wishing to keep it out of
    /// logs should use [`SrtConfig::to_url_redacted`].
    ///
    /// # Errors
    ///
    /// Any [`SrtError`] from [`SrtConfig::validate`].
    pub fn to_url(&self) -> Result<String, SrtError> {
        self.build_url(false)
    }

    /// Like [`SrtConfig::to_url`] but with the passphrase replaced by `***` — for
    /// logging / diagnostics.
    ///
    /// # Errors
    ///
    /// Any [`SrtError`] from [`SrtConfig::validate`].
    pub fn to_url_redacted(&self) -> Result<String, SrtError> {
        self.build_url(true)
    }

    /// Assemble the URL with optional passphrase redaction.
    fn build_url(&self, redact: bool) -> Result<String, SrtError> {
        self.validate()?;
        let mut params: Vec<(&str, String)> = vec![
            ("mode", self.mode.as_libav_token().to_owned()),
            ("latency", self.latency_ms.to_string()),
        ];
        if self.key_length.is_encrypted() {
            params.push(("pbkeylen", self.key_length.bytes().to_string()));
            if let Some(pp) = self.passphrase.as_deref() {
                let value = if redact {
                    "***".to_owned()
                } else {
                    pp.to_owned()
                };
                params.push(("passphrase", value));
            }
        }
        if let Some(sid) = self.stream_id.as_ref() {
            params.push(("streamid", percent_encode(sid.as_str())));
        }
        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        // IPv6-first: an IPv6 literal host must be bracketed (`srt://[::1]:port`)
        // so the `:` of the address is not read as the port separator. Already
        // bracketed or non-IPv6 hosts pass through unchanged.
        let host = bracket_ipv6_host(&self.host);
        Ok(format!("srt://{host}:{}?{query}", self.port))
    }
}

/// Wrap a bare IPv6 literal host in `[...]` for use in a `host:port` URL; leave
/// hostnames, IPv4 literals, and already-bracketed hosts untouched.
fn bracket_ipv6_host(host: &str) -> std::borrow::Cow<'_, str> {
    if host.starts_with('[') {
        return std::borrow::Cow::Borrowed(host);
    }
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        return std::borrow::Cow::Owned(format!("[{host}]"));
    }
    std::borrow::Cow::Borrowed(host)
}

/// Percent-encode the characters that would break an SRT URL query value
/// (`&`, `=`, `?`, `#`, space, and `%` itself). The SRT-AC stream-id syntax
/// (`#!::…`) needs `#` encoded.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'&' | b'=' | b'?' | b'#' | b'%' | b' ' => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0F));
            }
            other => match char::from_u32(u32::from(other)) {
                Some(c) => out.push(c),
                None => out.push('?'),
            },
        }
    }
    out
}

/// Map a nibble (`0..=15`) to its uppercase hex digit.
const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0 => '0',
        1 => '1',
        2 => '2',
        3 => '3',
        4 => '4',
        5 => '5',
        6 => '6',
        7 => '7',
        8 => '8',
        9 => '9',
        10 => 'A',
        11 => 'B',
        12 => 'C',
        13 => 'D',
        14 => 'E',
        _ => 'F',
    }
}
