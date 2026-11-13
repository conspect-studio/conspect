//! Cast stream tokens (ADR-0111, design §3): 32 bytes from the OS CSPRNG,
//! base64url without padding (43 characters), stored only as their SHA-256
//! digest and compared in constant time.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Raw token length in bytes.
pub const TOKEN_BYTES: usize = 32;
/// Encoded token length in characters (base64url, no padding).
pub const TOKEN_CHARS: usize = 43;

/// The SHA-256 digest of a token: the only form the registry keeps.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TokenDigest([u8; 32]);

impl std::fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenDigest(..)")
    }
}

impl TokenDigest {
    /// The digest of `token` (any text; an invalid token simply matches
    /// nothing).
    #[must_use]
    pub fn of(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }

    /// Constant-time equality.
    #[must_use]
    pub fn ct_matches(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

/// Fill `buf` from the OS CSPRNG. `uuid` v4 is getrandom-backed (the same
/// source `tls_setup/code.rs` uses); each 16-byte draw carries 122 random
/// bits, mixed through SHA-256 so every output byte is uniform.
fn random_bytes(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(16) {
        let mut hasher = Sha256::new();
        hasher.update(uuid::Uuid::new_v4().into_bytes());
        hasher.update(uuid::Uuid::new_v4().into_bytes());
        let digest = hasher.finalize();
        let n = chunk.len();
        chunk.copy_from_slice(digest.get(..n).unwrap_or_default());
    }
}

/// A per-process secret the tokens are derived with (never logged).
#[derive(Clone)]
pub struct Pepper([u8; 32]);

impl std::fmt::Debug for Pepper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pepper(..)")
    }
}

impl Pepper {
    /// A fresh random pepper.
    #[must_use]
    pub fn random() -> Self {
        let mut raw = [0u8; 32];
        random_bytes(&mut raw);
        Self(raw)
    }
}

/// The random seed a token is derived from (kept in the registry; useless
/// without the process [`Pepper`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Seed([u8; TOKEN_BYTES]);

impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seed(..)")
    }
}

/// The token of `seed`: base64url(HMAC-SHA256(pepper, seed)).
#[must_use]
pub fn derive(pepper: &Pepper, seed: &Seed) -> String {
    use hmac::{KeyInit, Mac};
    let mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(&pepper.0)
        .map(|mut mac| {
            mac.update(&seed.0);
            mac.finalize().into_bytes()
        })
        // HMAC accepts any key length; unreachable, but never panic.
        .unwrap_or_else(|_| Sha256::digest(seed.0));
    URL_SAFE_NO_PAD.encode(mac)
}

/// Mint a fresh token: `(seed, plaintext, digest)`. The registry keeps the
/// seed and the digest; the plaintext goes to the owner only.
#[must_use]
pub fn mint(pepper: &Pepper) -> (Seed, String, TokenDigest) {
    let mut raw = [0u8; TOKEN_BYTES];
    random_bytes(&mut raw);
    let seed = Seed(raw);
    let token = derive(pepper, &seed);
    let digest = TokenDigest::of(&token);
    (seed, token, digest)
}

/// Whether `token` has the shape of a minted token (43 base64url
/// characters). A cheap pre-check only; the lookup decides.
#[must_use]
pub fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_CHARS
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Whether `file` is a servable cast file name:
/// `^[A-Za-z0-9._-]{1,96}\.(m3u8|m4s|mp4)$` and no `..`.
#[must_use]
pub fn file_allowed(file: &str) -> bool {
    let Some((stem, ext)) = file.rsplit_once('.') else {
        return false;
    };
    (1..=96).contains(&stem.len())
        && matches!(ext, "m3u8" | "m4s" | "mp4")
        && !file.contains("..")
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_tokens_are_43_base64url_chars_and_distinct() {
        let pepper = Pepper::random();
        let (sa, a, da) = mint(&pepper);
        let (_, b, db) = mint(&pepper);
        assert_eq!(derive(&pepper, &sa), a);
        assert_ne!(derive(&Pepper::random(), &sa), a);
        assert!(well_formed(&a) && well_formed(&b));
        assert_ne!(a, b);
        assert!(!da.ct_matches(&db));
        assert!(TokenDigest::of(&a).ct_matches(&da));
        assert_eq!(format!("{da:?}"), "TokenDigest(..)");
    }

    #[test]
    fn file_allow_list() {
        for ok in [
            "master.m3u8",
            "video-g1.m3u8",
            "seg_00001.m4s",
            "init-g2.mp4",
        ] {
            assert!(file_allowed(ok), "{ok}");
        }
        for bad in [
            "..m3u8",
            "a..b.m4s",
            "x.ts",
            "x.vtt",
            ".m3u8",
            "a/b.m4s",
            "%2e%2e.m3u8",
            "a b.mp4",
            "master.m3u8.txt",
        ] {
            assert!(!file_allowed(bad), "{bad}");
        }
        assert!(!file_allowed(&format!("{}.m4s", "a".repeat(97))));
        assert!(file_allowed(&format!("{}.m4s", "a".repeat(96))));
    }
}
