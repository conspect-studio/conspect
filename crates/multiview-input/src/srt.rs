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
//! * [`StreamIdParams`] — the typed form of the `#!::key=value` SRT Access
//!   Control "Standard" stream-id grammar (SRT-1), parsed via `TryFrom` with an
//!   exhaustive `match` (never string-munged): reserved single-letter keys
//!   `u`/`r`/`h`/`s`, typed `t` ([`SrtAcType`]) and `m` ([`SrtAcMode`], default
//!   `request`), namespaced `user_*`/`multiview_*` extensions.
//! * [`SrtRejection`] — the SRT Access-Control rejection codes
//!   (`SRT_REJX_*`), reachable from [`SrtError::rejection`].
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
    /// A passphrase was outside SRT's valid length (10..=80 characters) while
    /// encryption was requested.
    #[error("srt passphrase length {0} is outside the valid range 10..=80")]
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

    /// This backend is not implemented by the Tier-0 `FFmpeg` transport.
    #[error("srt backend is unsupported until SRT-5")]
    UnsupportedBackend,

    /// An option has no corresponding `FFmpeg` 8.1.1 SRT URL parameter.
    #[error("srt option {0} is unsupported by FFmpeg 8.1.1")]
    UnsupportedOption(&'static str),

    /// A referenced passphrase could not be resolved at socket setup.
    #[error("srt passphrase reference could not be resolved")]
    SecretUnavailable,

    /// A stream id was not a parseable SRT Access-Control `#!::key=value`
    /// grammar (SRT-1): the grammar is typed ([`StreamIdParams`]), never
    /// string-munged, so an unrecognised key, a duplicate key, a malformed
    /// pair, or an unknown `t=`/`m=` token is rejected outright.
    #[error("srt stream id is not a parseable Access-Control grammar: {0}")]
    StreamIdUnparseable(String),
}

impl SrtError {
    /// The SRT Access-Control rejection code (`SRT_REJX_*`) this error maps
    /// to, when it is a stream-id rejection rather than a local validation
    /// failure.
    ///
    /// Only [`SrtError::StreamIdUnparseable`] carries a rejection today; other
    /// variants are local configuration errors with no `SRT_REJX` counterpart.
    #[must_use]
    pub const fn rejection(&self) -> Option<SrtRejection> {
        match self {
            Self::StreamIdUnparseable(_) => Some(SrtRejection::BadRequest),
            _ => None,
        }
    }
}

/// An SRT Access-Control rejection code (`SRT_REJX_*`), as logged/answered by
/// a listener that refuses a caller's stream id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SrtRejection {
    /// `SRT_REJX_BAD_REQUEST` (1400): the stream id is not a parseable
    /// Access-Control grammar.
    BadRequest,
    /// `SRT_REJX_UNAUTHORIZED` (1401): the caller is not permitted to access
    /// the requested resource.
    Unauthorized,
    /// `SRT_REJX_OVERLOAD` (1402): the listener has no capacity for another
    /// stream.
    Overload,
    /// `SRT_REJX_NOTFOUND` (1404): the requested resource does not exist.
    NotFound,
    /// `SRT_REJX_CONFLICT` (1409): the requested resource is already in use.
    Conflict,
}

impl SrtRejection {
    /// The numeric `SRT_REJX` code for this rejection.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::BadRequest => 1400,
            Self::Unauthorized => 1401,
            Self::Overload => 1402,
            Self::NotFound => 1404,
            Self::Conflict => 1409,
        }
    }
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

/// The SRT Access-Control request mode: the reserved `m` key of the
/// `#!::` stream-id grammar.
///
/// `request` is the protocol default when `m` is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum SrtAcMode {
    /// `request`: the caller wants to *receive* the resource.
    #[default]
    Request,
    /// `publish`: the caller wants to *send* to the listener.
    Publish,
    /// `bidirectional`: both directions on one connection.
    Bidirectional,
}

impl SrtAcMode {
    /// The grammar token for this mode (`m=…`).
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Publish => "publish",
            Self::Bidirectional => "bidirectional",
        }
    }

    /// Parse an `m=` grammar token into a [`SrtAcMode`].
    ///
    /// # Errors
    ///
    /// [`SrtError::StreamIdUnparseable`] when the token is not a known mode.
    pub fn from_token(token: &str) -> Result<Self, SrtError> {
        match token {
            "request" => Ok(Self::Request),
            "publish" => Ok(Self::Publish),
            "bidirectional" => Ok(Self::Bidirectional),
            other => Err(SrtError::StreamIdUnparseable(format!(
                "unknown m= token {other:?}"
            ))),
        }
    }
}

/// The SRT Access-Control stream type: the reserved `t` key of the `#!::`
/// stream-id grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SrtAcType {
    /// `stream`: a live media stream.
    Stream,
    /// `file`: a file transfer.
    File,
    /// `auth`: an authentication-only exchange (no media).
    Auth,
}

impl SrtAcType {
    /// The grammar token for this type (`t=…`).
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::File => "file",
            Self::Auth => "auth",
        }
    }

    /// Parse a `t=` grammar token into a [`SrtAcType`].
    ///
    /// # Errors
    ///
    /// [`SrtError::StreamIdUnparseable`] when the token is not a known type.
    pub fn from_token(token: &str) -> Result<Self, SrtError> {
        match token {
            "stream" => Ok(Self::Stream),
            "file" => Ok(Self::File),
            "auth" => Ok(Self::Auth),
            other => Err(SrtError::StreamIdUnparseable(format!(
                "unknown t= token {other:?}"
            ))),
        }
    }
}

/// The typed form of an SRT Access-Control "Standard" stream id
/// (`#!::key=value,key=value`) — SRT-1.
///
/// Parsed by `TryFrom` with an exhaustive `match` over the keys, never by
/// string-munging (ADR-0039 Decision 9). The reserved single-letter keys map
/// to typed fields — `u` (user), `r` (resource), `h` (host), `s` (session),
/// `t` ([`SrtAcType`]), `m` ([`SrtAcMode`], `request` when absent). Custom
/// keys must be namespaced (`user_*` or `multiview_*` with a nonempty suffix);
/// bare keys — single letters especially — are rejected as unparseable, which
/// a listener surfaces as [`SrtRejection::BadRequest`] (1400). Round-trips
/// through [`StreamId`] and [`SrtConfig::to_url`] with `#` kept percent-escaped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamIdParams {
    /// The `u` (user) value.
    user: Option<String>,
    /// The `r` (resource) value.
    resource: Option<String>,
    /// The `h` (host) value.
    host: Option<String>,
    /// The `s` (session id) value.
    session: Option<String>,
    /// The `t` (stream type) value.
    stream_type: Option<SrtAcType>,
    /// The `m` (mode) value; `None` means the grammar omitted `m`, and
    /// [`StreamIdParams::mode`] applies the protocol's `request` default.
    mode: Option<SrtAcMode>,
    /// The namespaced `user_*`/`multiview_*` extension keys, ordered by key.
    extensions: std::collections::BTreeMap<String, String>,
}

impl StreamIdParams {
    /// The prefix every SRT-AC "Standard" stream id carries.
    pub const PREFIX: &'static str = "#!::";

    /// The `u` (user) value.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// The `r` (resource) value.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }

    /// The `h` (host) value.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// The `s` (session id) value.
    #[must_use]
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The `t` (stream type) value.
    #[must_use]
    pub const fn stream_type(&self) -> Option<SrtAcType> {
        self.stream_type
    }

    /// The `m` (mode) value, applying the protocol's `request` default when
    /// the grammar omitted `m`.
    #[must_use]
    pub fn mode(&self) -> SrtAcMode {
        self.mode.unwrap_or(SrtAcMode::Request)
    }

    /// The namespaced `user_*`/`multiview_*` extension keys, ordered by key.
    #[must_use]
    pub fn extensions(&self) -> &std::collections::BTreeMap<String, String> {
        &self.extensions
    }

    /// Render the canonical `#!::` grammar string: reserved keys in
    /// `u`,`r`,`h`,`s`,`t`,`m` order, then extensions in key order. `m` is
    /// written only when the grammar carried it, so `parse → render` is the
    /// identity on values.
    ///
    /// # Errors
    ///
    /// [`SrtError::StreamIdTooLong`] when the rendered id would exceed
    /// [`StreamId::MAX_BYTES`].
    pub fn to_stream_id_string(&self) -> Result<String, SrtError> {
        let mut pairs: Vec<String> = Vec::new();
        if let Some(user) = self.user() {
            pairs.push(format!("u={user}"));
        }
        if let Some(resource) = self.resource() {
            pairs.push(format!("r={resource}"));
        }
        if let Some(host) = self.host() {
            pairs.push(format!("h={host}"));
        }
        if let Some(session) = self.session() {
            pairs.push(format!("s={session}"));
        }
        if let Some(stream_type) = self.stream_type {
            pairs.push(format!("t={}", stream_type.as_token()));
        }
        if let Some(mode) = self.mode {
            pairs.push(format!("m={}", mode.as_token()));
        }
        for (key, value) in &self.extensions {
            pairs.push(format!("{key}={value}"));
        }
        let id = format!("{}{}", Self::PREFIX, pairs.join(","));
        if id.len() > StreamId::MAX_BYTES {
            return Err(SrtError::StreamIdTooLong(id.len()));
        }
        Ok(id)
    }
}

impl TryFrom<&str> for StreamIdParams {
    type Error = SrtError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let Some(body) = value.strip_prefix(Self::PREFIX) else {
            return Err(SrtError::StreamIdUnparseable(
                "missing `#!::` prefix".to_owned(),
            ));
        };
        // The bare prefix is the empty grammar.
        if body.is_empty() {
            return Ok(Self::default());
        }
        let mut params = Self::default();
        for pair in body.split(',') {
            let Some((key, val)) = pair.split_once('=') else {
                return Err(SrtError::StreamIdUnparseable(format!(
                    "pair {pair:?} has no `=`"
                )));
            };
            if key.is_empty() {
                return Err(SrtError::StreamIdUnparseable(
                    "empty key before `=`".to_owned(),
                ));
            }
            // Exhaustive match over the grammar: the six reserved
            // single-letter keys, the two extension namespaces, and nothing
            // else — a bare custom key falls through to the error arm.
            match key {
                "u" => set_once(&mut params.user, "u", val)?,
                "r" => set_once(&mut params.resource, "r", val)?,
                "h" => set_once(&mut params.host, "h", val)?,
                "s" => set_once(&mut params.session, "s", val)?,
                "t" => {
                    if params.stream_type.is_some() {
                        return Err(duplicate_key("t"));
                    }
                    params.stream_type = Some(SrtAcType::from_token(val)?);
                }
                "m" => {
                    if params.mode.is_some() {
                        return Err(duplicate_key("m"));
                    }
                    params.mode = Some(SrtAcMode::from_token(val)?);
                }
                key if is_extension_key(key) => {
                    if params
                        .extensions
                        .insert(key.to_owned(), val.to_owned())
                        .is_some()
                    {
                        return Err(duplicate_key(key));
                    }
                }
                other => {
                    return Err(SrtError::StreamIdUnparseable(format!(
                        "key {other:?} is not a reserved key and not a namespaced user_*/multiview_* extension"
                    )));
                }
            }
        }
        Ok(params)
    }
}

impl TryFrom<&StreamIdParams> for StreamId {
    type Error = SrtError;

    fn try_from(params: &StreamIdParams) -> Result<Self, Self::Error> {
        StreamId::new(params.to_stream_id_string()?)
    }
}

impl TryFrom<&StreamId> for StreamIdParams {
    type Error = SrtError;

    fn try_from(stream_id: &StreamId) -> Result<Self, Self::Error> {
        Self::try_from(stream_id.as_str())
    }
}

/// Set a reserved string slot once; a second occurrence of a key is
/// unparseable (the grammar forbids duplicate keys).
fn set_once(slot: &mut Option<String>, key: &'static str, val: &str) -> Result<(), SrtError> {
    if slot.is_some() {
        return Err(duplicate_key(key));
    }
    *slot = Some(val.to_owned());
    Ok(())
}

/// The grammar error for a key appearing more than once.
fn duplicate_key(key: &str) -> SrtError {
    SrtError::StreamIdUnparseable(format!("duplicate key {key:?}"))
}

/// Whether `key` is a namespaced custom key: `user_*` or `multiview_*` with a
/// nonempty suffix. Custom keys MUST be namespaced so they can never collide
/// with current or future reserved single-letter keys.
fn is_extension_key(key: &str) -> bool {
    key.strip_prefix("user_")
        .is_some_and(|suffix| !suffix.is_empty())
        || key
            .strip_prefix("multiview_")
            .is_some_and(|suffix| !suffix.is_empty())
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
    /// The receive latency in **milliseconds** (libsrt's native
    /// `SRTO_RCVLATENCY` unit; SRT `latency`/`rcvlatency`).
    ///
    /// **Unit boundary (verified defect, ADR-0039 §3):** libsrt's native API
    /// takes milliseconds, but `FFmpeg`'s `libsrt` `AVOption` `latency=` takes
    /// **microseconds** (`latency=200000` ⇒ 200 ms). [`SrtConfig::to_url`]
    /// emits microseconds after an exact `×1000` conversion; the model stays
    /// in milliseconds, never float (invariant #3).
    pub latency_ms: u32,
    /// `FFmpeg`'s `oheadbw` percentage (not a time quantity).
    pub oheadbw_pct: Option<u32>,
    /// `FFmpeg`'s `maxbw` in bytes per second.
    pub maxbw: Option<i64>,
    /// `FFmpeg`'s `connect_timeout` in milliseconds.
    pub conn_timeout_ms: Option<u32>,
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
            oheadbw_pct: None,
            maxbw: None,
            conn_timeout_ms: None,
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
                // libsrt accepts 10..=80 characters (`HAICRYPT_SECRET_MAX_SZ = 80`);
                // Haivision's product docs say 79, which the old bound here copied.
                // The FFmpeg wrapper inherits the 79 cap, so an 80-character
                // passphrase is valid at this model layer but a Tier-0 (FFmpeg)
                // backend rejects it at connect time; the wiring that resolves a
                // backend must therefore warn (never error) on the 80-character
                // case (ADR-0039 §5).
                if pp.len() < 10 || pp.len() > 80 {
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
    /// `pbkeylen`, `passphrase`, `streamid`). The passphrase is included as a
    /// percent-encoded query value because the libav demuxer requires it; callers wishing to keep it out of
    /// logs should use [`SrtConfig::to_url_redacted`].
    ///
    /// **Latency units:** [`latency_ms`](SrtConfig::latency_ms) stays in
    /// milliseconds in this model; the emitted libav `latency=` is converted
    /// exactly to **microseconds** (`×1000`, ADR-0039 §3).
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
            ("latency", (u64::from(self.latency_ms) * 1000).to_string()),
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
            params.push(("streamid", sid.as_str().to_owned()));
        }
        if let Some(pct) = self.oheadbw_pct {
            params.push(("oheadbw", pct.to_string()));
        }
        if let Some(maxbw) = self.maxbw {
            params.push(("maxbw", maxbw.to_string()));
        }
        if let Some(timeout) = self.conn_timeout_ms {
            params.push(("connect_timeout", timeout.to_string()));
        }
        let query = params
            .iter()
            .map(|(k, v)| {
                if redact && *k == "passphrase" {
                    format!("{k}=***")
                } else {
                    format!("{k}={}", percent_encode(v))
                }
            })
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

/// Strict RFC 3986 query-value encoding: preserve only unreserved ASCII bytes.
/// Encode every other UTF-8 byte as uppercase `%XX`, including `+` (which
/// libav's `ff_urldecode` would otherwise turn into a space).
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0F));
            }
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
