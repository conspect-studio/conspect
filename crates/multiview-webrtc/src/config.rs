//! The plain endpoint configuration the cli maps the `[webrtc]` config section
//! into.
//!
//! Per [ADR-0048 §1/§9](../../docs/decisions/ADR-0048.md) `multiview-webrtc` does
//! **not** depend on `multiview-config`: the cli reads the config document and
//! constructs this plain [`EndpointConfig`], so the crate never grows a config
//! dependency. This module owns the dual-stack bind, the advertised addresses,
//! session caps, GC horizons, and the **ICE-server list (STUN + TURN)** the
//! operator requires.
//!
//! ## Secret field naming (the redactor contract)
//!
//! TURN credentials are SECRETS. The control plane's config redactor
//! (`multiview-control::redact_config`) drops any JSON key containing `secret`,
//! `password`, `token`, `credential`, `api_key`, or equal to `key`/`auth`. The
//! credential fields here are named so that — when the cli serializes the
//! `[webrtc]` section — they are caught: [`TurnCredentials::password`],
//! [`TurnCredentials::static_auth_secret`]. The TURN `username` is not a secret
//! (usernames are not redacted), matching coturn's own posture.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// The default single UDP media port (ADR-0048 §4), bound dual-stack `[::]`.
pub const DEFAULT_UDP_PORT: u16 = 8189;

/// The default cap on preview + output-viewer sessions (ADR-0048 §8).
pub const DEFAULT_MAX_SESSIONS: u32 = 64;

/// The default idle/ICE-disconnect GC horizon (ADR-0048 §8).
pub const DEFAULT_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The default closed-session tombstone eviction delay (ADR-0048 §8).
pub const DEFAULT_TOMBSTONE_TTL: Duration = Duration::from_secs(60);

/// The endpoint configuration.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    /// The single UDP media port (bound dual-stack `[::]`, `IPV6_V6ONLY=false`).
    pub udp_port: u16,
    /// Extra candidate addresses for NAT 1:1 / Docker (IPv6 listed first).
    pub advertised_addresses: Vec<IpAddr>,
    /// Hard cap on preview + output-viewer sessions (ingest/push sit outside it).
    pub max_sessions: u32,
    /// Idle/ICE-disconnect GC horizon.
    pub session_idle_timeout: Duration,
    /// Closed-session tombstone eviction delay.
    pub tombstone_ttl: Duration,
    /// CORS allow-list applied only to the media-signalling routes.
    pub cors_allow_origins: Vec<String>,
    /// The ICE-server list (STUN + TURN) — the operator's TURN requirement.
    pub ice_servers: Vec<IceServer>,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self {
            udp_port: DEFAULT_UDP_PORT,
            advertised_addresses: Vec::new(),
            max_sessions: DEFAULT_MAX_SESSIONS,
            session_idle_timeout: DEFAULT_SESSION_IDLE_TIMEOUT,
            tombstone_ttl: DEFAULT_TOMBSTONE_TTL,
            cors_allow_origins: vec!["*".to_owned()],
            ice_servers: Vec::new(),
        }
    }
}

impl EndpointConfig {
    /// The dual-stack bind address `[::]:udp_port`.
    #[must_use]
    pub fn bind_addr(&self) -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), self.udp_port)
    }

    /// Validate the configuration, returning the first problem found.
    ///
    /// # Errors
    ///
    /// [`crate::WebRtcError::Config`] if a TURN server is configured without
    /// usable credentials, or `max_sessions` is zero.
    pub fn validate(&self) -> crate::Result<()> {
        if self.max_sessions == 0 {
            return Err(crate::WebRtcError::Config(
                "max_sessions must be at least 1".to_owned(),
            ));
        }
        for server in &self.ice_servers {
            server.validate()?;
        }
        Ok(())
    }

    /// Just the TURN servers (those with relay credentials), in config order.
    pub fn turn_servers(&self) -> impl Iterator<Item = &IceServer> {
        self.ice_servers
            .iter()
            .filter(|s| s.kind == IceServerKind::Turn)
    }
}

/// Whether an ICE server offers STUN binding only, or TURN relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IceServerKind {
    /// STUN binding only (server-reflexive discovery).
    Stun,
    /// TURN relay allocation (RFC 5766/8656).
    Turn,
}

/// One configured ICE server (`stun:` / `turn:` / `turns:`).
#[derive(Debug, Clone)]
pub struct IceServer {
    /// Whether this is a STUN or TURN server.
    pub kind: IceServerKind,
    /// The server transport address.
    pub addr: SocketAddr,
    /// TURN credentials (required for [`IceServerKind::Turn`], ignored for STUN).
    pub credentials: Option<TurnCredentials>,
}

impl IceServer {
    /// A STUN server (binding only).
    #[must_use]
    pub fn stun(addr: SocketAddr) -> Self {
        Self {
            kind: IceServerKind::Stun,
            addr,
            credentials: None,
        }
    }

    /// A TURN server with the given credentials.
    #[must_use]
    pub fn turn(addr: SocketAddr, credentials: TurnCredentials) -> Self {
        Self {
            kind: IceServerKind::Turn,
            addr,
            credentials: Some(credentials),
        }
    }

    /// Validate this server's credentials.
    ///
    /// # Errors
    ///
    /// [`crate::WebRtcError::Config`] if a TURN server has no credentials.
    pub fn validate(&self) -> crate::Result<()> {
        if self.kind == IceServerKind::Turn && self.credentials.is_none() {
            return Err(crate::WebRtcError::Config(format!(
                "TURN server {} requires credentials",
                self.addr
            )));
        }
        Ok(())
    }
}

/// TURN credentials — long-term (static) **or** coturn-style ephemeral REST/HMAC.
///
/// The `username` is not secret. `password` and `static_auth_secret` are SECRETS
/// whose field names are caught by the config redactor (see the module docs).
#[derive(Clone, PartialEq, Eq)]
pub struct TurnCredentials {
    /// Long-term username (or the `name` part of an ephemeral REST username).
    pub username: String,
    /// The long-term cleartext password. `None` when using ephemeral REST creds.
    pub password: Option<String>,
    /// The coturn `use-auth-secret` shared secret for ephemeral REST credentials.
    /// `None` for static long-term credentials.
    pub static_auth_secret: Option<String>,
    /// The realm, if pre-known (otherwise learned from the server's `401`).
    pub realm: Option<String>,
    /// For ephemeral REST credentials: how long a derived credential is valid.
    pub ttl: Duration,
}

impl std::fmt::Debug for TurnCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret material.
        f.debug_struct("TurnCredentials")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field(
                "static_auth_secret",
                &self.static_auth_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("realm", &self.realm)
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl TurnCredentials {
    /// Static long-term credentials.
    #[must_use]
    pub fn long_term(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: Some(password.into()),
            static_auth_secret: None,
            realm: None,
            ttl: Duration::from_secs(3600),
        }
    }

    /// Ephemeral REST/HMAC credentials (coturn `use-auth-secret`). The credential
    /// is derived per allocation from `static_auth_secret` and a fresh expiry.
    #[must_use]
    pub fn ephemeral_rest(name: impl Into<String>, static_auth_secret: impl Into<String>) -> Self {
        Self {
            username: name.into(),
            password: None,
            static_auth_secret: Some(static_auth_secret.into()),
            realm: None,
            ttl: Duration::from_secs(3600),
        }
    }

    /// Resolve a concrete [`crate::turn::TurnCredential`] for an allocation
    /// expiring `ttl` from `now_unix_secs`.
    ///
    /// For long-term creds this is the static username/password. For ephemeral
    /// REST creds it derives `username = "<expiry>:<name>"`,
    /// `password = base64(HMAC-SHA1(secret, username))`.
    #[must_use]
    pub fn resolve(&self, now_unix_secs: u64) -> crate::turn::TurnCredential {
        use crate::turn::TurnCredential;
        match (&self.password, &self.static_auth_secret) {
            (Some(password), _) => {
                TurnCredential::static_credential(&self.username, password, self.realm.clone())
            }
            (None, Some(secret)) => {
                let expiry = now_unix_secs.saturating_add(self.ttl.as_secs());
                let username = TurnCredential::rest_username(expiry, &self.username);
                let mut cred = TurnCredential::ephemeral(username, secret.as_bytes());
                cred.realm.clone_from(&self.realm);
                cred
            }
            // No usable credential material — config validation rejects this, so
            // an empty static credential is the safe degenerate fallback.
            (None, None) => {
                TurnCredential::static_credential(&self.username, "", self.realm.clone())
            }
        }
    }
}
