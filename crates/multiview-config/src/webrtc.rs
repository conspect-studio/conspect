//! The optional top-level `[webrtc]` section (ADR-0048 §9): the knobs for the
//! shared WebRTC transport endpoint — one process-wide, dual-stack UDP media
//! socket (`[::]`, `IPV6_V6ONLY=false`, never `0.0.0.0` — ADR-0042) that every
//! WebRTC session multiplexes onto: WHIP ingest publishers (ADR-T014), WHEP
//! preview sessions, WHEP output viewers and the outbound `whip_push` client
//! (ADR-0049).
//!
//! An **absent** section yields a fully-defaulted [`WebrtcConfig`], and a
//! default-valued section (or field) does not serialize — the document only
//! carries what the operator changed. The cli maps this onto the
//! `multiview-webrtc` crate's plain `EndpointConfig` (`multiview-webrtc` never
//! depends on this crate, ADR-0048 §1/§9).

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;

/// A duration as an **explicit-unit** string (`"30s"`, `"1500ms"`, `"2m"`),
/// carried as exact whole milliseconds.
///
/// A bare TOML/JSON number (e.g. `30`) deliberately fails to deserialize: the
/// unit is always explicit, matching ADR-0048 §9's
/// `session_idle_timeout = "30s"` form, and a fractional value (`"1.5s"`) is
/// rejected — express it in the next-finer unit (`"1500ms"`). Re-serializes to
/// the coarsest exact unit so a parsed document round-trips cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationString(u64);

impl DurationString {
    /// Build a duration from whole milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// The exact duration in whole milliseconds.
    #[must_use]
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// The value as a [`std::time::Duration`].
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        Duration::from_millis(self.0)
    }
}

impl FromStr for DurationString {
    type Err = ConfigError;

    /// Parse `"<integer><ms|s|m>"` into an exact millisecond duration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidDuration`] when the unit is missing or
    /// unknown, the magnitude is not a whole non-negative integer, or the
    /// value overflows a `u64` of milliseconds.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let invalid = |reason: &str| ConfigError::InvalidDuration {
            value: trimmed.to_owned(),
            reason: reason.to_owned(),
        };
        // `ms` must be tried before `s` (it is a suffix of it).
        let (digits, scale_ms): (&str, u64) = if let Some(d) = trimmed.strip_suffix("ms") {
            (d, 1)
        } else if let Some(d) = trimmed.strip_suffix('s') {
            (d, 1_000)
        } else if let Some(d) = trimmed.strip_suffix('m') {
            (d, 60_000)
        } else {
            return Err(invalid(
                "expected an explicit unit suffix: `ms`, `s`, or `m` (e.g. \"30s\")",
            ));
        };
        if digits.is_empty() {
            return Err(invalid("missing a magnitude before the unit"));
        }
        let magnitude: u64 = digits.parse().map_err(|_| {
            invalid("magnitude must be a whole non-negative integer (no floats — use a finer unit)")
        })?;
        let millis = magnitude
            .checked_mul(scale_ms)
            .ok_or_else(|| invalid("value overflows the millisecond range"))?;
        Ok(Self(millis))
    }
}

impl fmt::Display for DurationString {
    /// Render to the coarsest unit that represents the value exactly.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 != 0 && self.0 % 60_000 == 0 {
            write!(f, "{}m", self.0 / 60_000)
        } else if self.0 != 0 && self.0 % 1_000 == 0 {
            write!(f, "{}s", self.0 / 1_000)
        } else {
            write!(f, "{}ms", self.0)
        }
    }
}

impl Serialize for DurationString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DurationString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Visitor that accepts only strings (a bare number is a wrong type).
        struct DurationVisitor;
        impl Visitor<'_> for DurationVisitor {
            type Value = DurationString;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an explicit-unit duration string like \"30s\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_str(DurationVisitor)
    }
}

/// Default `webrtc.udp_port`: the single shared media socket (ADR-0048 §4).
const DEFAULT_UDP_PORT: u16 = 8189;

/// Default `webrtc.max_sessions`: the preview + output-viewer pool cap (§8).
const DEFAULT_MAX_SESSIONS: u32 = 64;

/// Default `webrtc.session_idle_timeout`: 30 s of no media/STUN closes a
/// session (§8).
const DEFAULT_SESSION_IDLE_TIMEOUT: DurationString = DurationString::from_millis(30_000);

/// Default `webrtc.udp_port` (serde shape).
const fn default_udp_port() -> u16 {
    DEFAULT_UDP_PORT
}

/// Default `webrtc.max_sessions` (serde shape).
const fn default_max_sessions() -> u32 {
    DEFAULT_MAX_SESSIONS
}

/// Default `webrtc.session_idle_timeout` (serde shape).
const fn default_session_idle_timeout() -> DurationString {
    DEFAULT_SESSION_IDLE_TIMEOUT
}

/// Default `webrtc.cors_allow_origins`: every origin (`["*"]`, §9) — the
/// media-signalling routes are Bearer/API-key protected regardless.
fn default_cors_allow_origins() -> Vec<String> {
    vec!["*".to_owned()]
}

/// Skip-serializing predicate for the default `udp_port`.
// serde's `skip_serializing_if` contract calls the predicate with the field by
// reference; the derive fixes the signature, so the by-value shape the lint
// asks for cannot be used here.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_udp_port(port: &u16) -> bool {
    *port == DEFAULT_UDP_PORT
}

/// Skip-serializing predicate for the default `max_sessions`.
// serde's `skip_serializing_if` contract calls the predicate with the field by
// reference (see `is_default_udp_port`).
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_max_sessions(sessions: &u32) -> bool {
    *sessions == DEFAULT_MAX_SESSIONS
}

/// Skip-serializing predicate for the default `session_idle_timeout`.
// serde's `skip_serializing_if` contract calls the predicate with the field by
// reference (see `is_default_udp_port`).
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_session_idle_timeout(timeout: &DurationString) -> bool {
    *timeout == DEFAULT_SESSION_IDLE_TIMEOUT
}

/// Skip-serializing predicate for the default `cors_allow_origins` (`["*"]`).
fn is_default_cors_allow_origins(origins: &[String]) -> bool {
    matches!(origins, [only] if only == "*")
}

/// The top-level `[webrtc]` section (ADR-0048 §9): shared-endpoint transport
/// knobs for **every** WebRTC role — WHIP ingest (ADR-T014), WHEP preview,
/// WHEP output viewers and the `whip_push` client (ADR-0049).
///
/// Absent ⇒ all defaults (this struct's [`Default`]); the endpoint only starts
/// when a `webrtc`-native build has a consumer. A default-valued section (or
/// field) does not serialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WebrtcConfig {
    /// The single shared UDP media socket port, bound dual-stack `[::]` with
    /// `IPV6_V6ONLY=false` — never `0.0.0.0` (ADR-0048 §4, ADR-0042). All
    /// sessions multiplex on it (STUN routes by ICE ufrag, the rest by learned
    /// remote address), so this one port is the whole media firewall story.
    /// Default `8189`.
    #[serde(
        default = "default_udp_port",
        skip_serializing_if = "is_default_udp_port"
    )]
    pub udp_port: u16,
    /// Extra candidate addresses appended to the gathered host candidates —
    /// the NAT 1:1 / Docker additional-hosts pattern (ADR-0048 §5). Bare IP
    /// literals or hostnames (no brackets, no port), IPv6 listed first
    /// (ADR-0042): e.g. `["2001:db8::15", "192.0.2.15"]`. Default empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advertised_addresses: Vec<String>,
    /// Hard cap on **preview + output-viewer** sessions only. WHIP-ingest
    /// sessions (bounded by the count of configured `webrtc` sources) and the
    /// `whip_push` client session (bounded by configured outputs) are admitted
    /// **outside** this pool, so a viewer flood can never starve a publisher
    /// (ADR-0048 §8). Default `64`.
    #[serde(
        default = "default_max_sessions",
        skip_serializing_if = "is_default_max_sessions"
    )]
    pub max_sessions: u32,
    /// Session idle GC horizon: no media/STUN activity for this long closes
    /// the session (its tombstone is evicted 60 s later — ADR-0048 §8). An
    /// explicit-unit duration string (`"30s"`, `"1500ms"`, `"2m"`). Default
    /// `"30s"`.
    #[serde(
        default = "default_session_idle_timeout",
        skip_serializing_if = "is_default_session_idle_timeout"
    )]
    pub session_idle_timeout: DurationString,
    /// CORS allow-list applied **only** to the media-signalling routes (WHIP,
    /// WHEP, preview-WHEP, capabilities), with `Access-Control-Allow-Headers:
    /// authorization, content-type` and `Access-Control-Expose-Headers:
    /// location, link` (ADR-0048 §9). Default `["*"]`.
    #[serde(
        default = "default_cors_allow_origins",
        skip_serializing_if = "is_default_cors_allow_origins"
    )]
    pub cors_allow_origins: Vec<String>,
    /// The STUN + TURN servers the endpoint uses for NAT traversal (ADR-0048
    /// §5.1 — the operator's in-crate TURN client). Each entry is a `stun:` /
    /// `turn:` / `turns:` URL plus, for TURN, the credentials (long-term
    /// `username`/`password` or coturn-style ephemeral REST `username` +
    /// `static_auth_secret`). The cli maps these onto the crate's plain
    /// `EndpointConfig` ICE-server list. Default empty (host candidates +
    /// `advertised_addresses` only — the self-hosted / port-forwarded case).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ice_servers: Vec<IceServerConfig>,
}

/// Whether an [`IceServerConfig`] entry is a STUN (binding-only) or a TURN
/// (relay) server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IceServerKindConfig {
    /// STUN binding only (server-reflexive discovery).
    Stun,
    /// TURN relay allocation (RFC 5766 / RFC 8656).
    Turn,
}

/// One configured ICE server (`stun:` / `turn:` / `turns:`), with optional TURN
/// credentials (ADR-0048 §5.1).
///
/// The `password` and `static_auth_secret` fields are inline cleartext secrets.
/// They are redacted on every outward-facing control-plane surface that renders
/// this config: the support bundle (`multiview-control::redact_config` drops any
/// secret-named key entirely) and the config-as-code export
/// (`multiview-control::redact_config_for_export` replaces the value with a
/// `<redacted>` placeholder, keeping the document re-importable). The `username`
/// is not a secret (matching coturn's posture).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IceServerConfig {
    /// STUN or TURN.
    pub kind: IceServerKindConfig,
    /// The server URL (e.g. `stun:[2001:db8::53]:3478`,
    /// `turn:[2001:db8::55]:3478`). IPv6 literals are bracketed (ADR-0042).
    pub url: String,
    /// TURN long-term username, or the `name` part of an ephemeral-REST
    /// username. Required (with credentials) for a TURN server, ignored for STUN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// TURN long-term cleartext password (RFC 5766 §4). A **secret** — the field
    /// name is redactor-caught. `None` when using ephemeral REST credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// The coturn `use-auth-secret` shared secret for ephemeral REST credentials
    /// (`draft-uberti-behave-turn-rest-00`). A **secret** — the field name is
    /// redactor-caught. `None` for static long-term credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_auth_secret: Option<String>,
    /// The TURN realm, if pre-known (otherwise learned from the server's `401`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
}

impl IceServerConfig {
    /// Validate this ICE-server entry: a non-empty URL, and — for TURN — usable
    /// credentials (a `password` or a `static_auth_secret`).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.url.trim().is_empty() {
            return Err(ConfigError::Validation(
                "webrtc.ice_servers entry has an empty url (e.g. \"stun:[2001:db8::53]:3478\")"
                    .to_owned(),
            ));
        }
        if self.kind == IceServerKindConfig::Turn
            && self.password.is_none()
            && self.static_auth_secret.is_none()
        {
            return Err(ConfigError::Validation(format!(
                "webrtc.ice_servers TURN server {:?} requires credentials \
                 (a password, or a static_auth_secret for ephemeral REST)",
                self.url
            )));
        }
        Ok(())
    }
}

impl Default for WebrtcConfig {
    /// The fully-defaulted section an absent `[webrtc]` table yields
    /// (ADR-0048 §9): port `8189`, no advertised addresses, `64` sessions,
    /// `"30s"` idle timeout, CORS `["*"]`.
    fn default() -> Self {
        Self {
            udp_port: DEFAULT_UDP_PORT,
            advertised_addresses: Vec::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            cors_allow_origins: default_cors_allow_origins(),
            ice_servers: Vec::new(),
        }
    }
}

impl WebrtcConfig {
    /// Whether every field carries its default — the whole-section
    /// skip-serializing predicate ([`crate::MultiviewConfig`] omits a
    /// default-valued `[webrtc]` table entirely).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Validate this section's semantics so a config that validates cannot
    /// fail at endpoint start: a bindable non-zero port, a non-empty session
    /// pool, a non-zero GC horizon, and advertised addresses / CORS origins
    /// that are structurally usable.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the violated rule.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.udp_port == 0 {
            return Err(ConfigError::Validation(
                "webrtc.udp_port must not be 0: the shared media socket needs a stable, \
                 forwardable port (default 8189)"
                    .to_owned(),
            ));
        }
        if self.max_sessions == 0 {
            return Err(ConfigError::Validation(
                "webrtc.max_sessions must be >= 1 (it caps preview + output-viewer sessions)"
                    .to_owned(),
            ));
        }
        if self.session_idle_timeout.millis() == 0 {
            return Err(ConfigError::Validation(
                "webrtc.session_idle_timeout must be non-zero (a zero horizon would GC every \
                 session immediately)"
                    .to_owned(),
            ));
        }
        for address in &self.advertised_addresses {
            validate_advertised_address(address)?;
        }
        for origin in &self.cors_allow_origins {
            if origin.is_empty() {
                return Err(ConfigError::Validation(
                    "webrtc.cors_allow_origins contains an empty origin (use \"*\" to allow \
                     every origin)"
                        .to_owned(),
                ));
            }
        }
        for server in &self.ice_servers {
            server.validate()?;
        }
        Ok(())
    }
}

/// Validate one `webrtc.advertised_addresses` entry: a bare IP literal
/// (`2001:db8::15`, `192.0.2.15`) or a hostname (`media.example.net`) —
/// candidate addresses carry no brackets and no port.
fn validate_advertised_address(address: &str) -> Result<(), ConfigError> {
    if address.parse::<IpAddr>().is_ok() || is_hostname(address) {
        return Ok(());
    }
    Err(ConfigError::Validation(format!(
        "webrtc.advertised_addresses entry {address:?} is neither an IP literal nor a hostname \
         (IPv6 literals are bare here — no brackets, no port: \"2001:db8::15\")"
    )))
}

/// Whether `s` is a syntactically valid hostname: dot-separated labels of
/// 1–63 ASCII alphanumerics/hyphens, no leading/trailing hyphen, ≤253 chars
/// total (RFC 1123 shape — resolvability is a runtime concern).
fn is_hostname(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}
