//! TURN long-term and ephemeral (coturn-style REST) credentials.
//!
//! TURN's long-term credential mechanism (RFC 5766 §4) authenticates each request
//! with a USERNAME / REALM / NONCE and a MESSAGE-INTEGRITY keyed by
//! `MD5(username:realm:password)`. Two ways to obtain that username/password:
//!
//! * **Static** — a fixed `username` + `password` (a configured long-term
//!   account on the TURN server).
//! * **Ephemeral REST / HMAC** — the coturn `use-auth-secret` scheme
//!   (`draft-uberti-behave-turn-rest-00`): a time-limited username
//!   `"<expiry-unix>:<name>"` whose password is
//!   `base64(HMAC-SHA1(shared_secret, username))`. The shared secret never
//!   leaves the server side in production; here the cli supplies it so the client
//!   can derive a fresh credential per allocation. The shared secret is a SECRET
//!   (config field named so the redactor strips it — see [`crate::config`]).

use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// A resolved TURN long-term credential: the `username`/`password` pair plus the
/// realm (filled from the server's `401` challenge if empty).
#[derive(Clone, PartialEq, Eq)]
pub struct TurnCredential {
    /// The USERNAME attribute value.
    pub username: String,
    /// The cleartext password used to derive the long-term key.
    pub password: String,
    /// The REALM, if known ahead of the server's challenge (usually learned from
    /// the `401`).
    pub realm: Option<String>,
}

impl std::fmt::Debug for TurnCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the password.
        f.debug_struct("TurnCredential")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("realm", &self.realm)
            .finish()
    }
}

impl TurnCredential {
    /// A static long-term credential.
    #[must_use]
    pub fn static_credential(
        username: impl Into<String>,
        password: impl Into<String>,
        realm: Option<String>,
    ) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
            realm,
        }
    }

    /// Derive a coturn-style ephemeral REST credential.
    ///
    /// `username` is `"<expiry-unix-seconds>:<name>"` (the caller composes the
    /// expiry); the password is `base64(HMAC-SHA1(shared_secret, username))`.
    #[must_use]
    pub fn ephemeral(username: impl Into<String>, shared_secret: &[u8]) -> Self {
        type HmacSha1 = Hmac<Sha1>;
        let username = username.into();
        let password = match HmacSha1::new_from_slice(shared_secret) {
            Ok(mut mac) => {
                mac.update(username.as_bytes());
                base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
            }
            // HMAC accepts any key length; this branch is unreachable in practice.
            Err(_e) => String::new(),
        };
        Self {
            username,
            password,
            realm: None,
        }
    }

    /// Compose an ephemeral REST username `"<expiry>:<name>"` from an absolute
    /// unix expiry timestamp and a logical name.
    #[must_use]
    pub fn rest_username(expiry_unix_secs: u64, name: &str) -> String {
        format!("{expiry_unix_secs}:{name}")
    }
}
