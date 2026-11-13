//! One-time setup codes, setup sessions and the failed-attempt limiter
//! (ADR-0107 section 5a "Setup authentication").
//!
//! * A setup code is 8 words drawn uniformly from the BIP-39 English list
//!   (2048 words, 88 bits). It is minted per boot and on
//!   `multiview tls setup-code`, valid one hour, and single use: redeeming it
//!   creates one session and burns it. Minting a new code replaces the old.
//! * A session is a random 32-byte token (cookie), valid one hour, honoured
//!   only while the front door is in `setup`.
//! * Five failed redemptions per source (an IPv4 address, or an IPv6 /64) in a
//!   15-minute window lock that source for 15 minutes. The table is bounded
//!   (LRU, [`LIMITER_CAPACITY`] entries).
//!
//! Comparisons are constant time. Codes and session tokens are never logged by
//! this module, and `Debug` is redacted.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The embedded BIP-39 English word list (MIT, bitcoin/bips; see
/// `THIRD-PARTY-NOTICES.md`). 2048 lowercase ASCII words.
const WORDS_TEXT: &str = include_str!("bip39-english.txt");
/// Words in a setup code.
pub const CODE_WORDS: usize = 8;
/// Setup code lifetime.
pub const CODE_TTL: Duration = Duration::from_secs(60 * 60);
/// Setup session lifetime.
pub const SESSION_TTL: Duration = Duration::from_secs(60 * 60);
/// Failed attempts per source before a lockout.
pub const MAX_FAILURES: u32 = 5;
/// The failed-attempt window.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
/// Lockout length once [`MAX_FAILURES`] is reached.
pub const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Largest number of sources the limiter tracks (LRU beyond it).
pub const LIMITER_CAPACITY: usize = 4096;
/// Most live setup sessions (oldest evicted beyond it).
pub const SESSION_CAPACITY: usize = 64;

/// A monotonic clock, injectable for tests.
pub type Clock = Arc<dyn Fn() -> std::time::Instant + Send + Sync>;

fn words() -> impl Iterator<Item = &'static str> {
    WORDS_TEXT.lines().map(str::trim).filter(|w| !w.is_empty())
}

fn word_list() -> &'static [&'static str] {
    static LIST: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| words().collect())
}

/// Fill `buf` from the OS CSPRNG (uuid v4 is getrandom-backed).
fn random_bytes(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(16) {
        let bytes = uuid::Uuid::new_v4().into_bytes();
        // A v4 UUID carries 122 random bits; 6 bits are fixed version/variant.
        // Mixing it through SHA-256 spreads the randomness over every byte.
        let digest = Sha256::digest(bytes);
        let n = chunk.len();
        chunk.copy_from_slice(digest.get(..n).unwrap_or_default());
    }
}

/// Draw a uniform index below 2048 (11 bits, no modulo bias).
fn random_word_index() -> usize {
    let mut b = [0u8; 2];
    random_bytes(&mut b);
    usize::from(u16::from_le_bytes(b) & 0x07ff)
}

/// Normalise a typed code: lowercase, words separated by single spaces
/// (hyphens, commas and repeated whitespace accepted as separators).
#[must_use]
pub fn normalize(code: &str) -> String {
    code.split(|c: char| c.is_whitespace() || c == '-' || c == ',')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The /64 bucket for IPv6, the address itself for IPv4 (and IPv4-mapped).
#[must_use]
pub fn source_bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
    }
}

struct Code {
    digest: [u8; 32],
    expires: std::time::Instant,
}

struct Session {
    expires: std::time::Instant,
}

#[derive(Clone, Copy)]
struct Bucket {
    failures: u32,
    window_start: std::time::Instant,
    locked_until: Option<std::time::Instant>,
    last_seen: std::time::Instant,
}

/// Why a redemption failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemError {
    /// Wrong, expired or already used code (counts as a failure).
    Invalid,
    /// The source is locked out; carries the remaining lockout.
    Locked(Duration),
}

#[derive(Default)]
struct Inner {
    code: Option<Code>,
    sessions: HashMap<[u8; 32], Session>,
    buckets: HashMap<IpAddr, Bucket>,
}

/// The setup-code authority for one run.
pub struct SetupAuth {
    inner: Mutex<Inner>,
    clock: Clock,
}

impl std::fmt::Debug for SetupAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetupAuth([redacted])")
    }
}

impl Default for SetupAuth {
    fn default() -> Self {
        Self::new()
    }
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

impl SetupAuth {
    /// A fresh authority on the real monotonic clock, with no code yet.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(Arc::new(std::time::Instant::now))
    }

    /// A fresh authority on an injected clock (tests).
    #[must_use]
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mint a new code (replacing any previous one) and return it once.
    #[must_use]
    pub fn mint(&self) -> String {
        let list = word_list();
        let code = (0..CODE_WORDS)
            .map(|_| list.get(random_word_index()).copied().unwrap_or("abandon"))
            .collect::<Vec<_>>()
            .join(" ");
        let now = (self.clock)();
        self.lock().code = Some(Code {
            digest: digest(&code),
            expires: now + CODE_TTL,
        });
        code
    }

    /// Redeem `code` from `peer`: on success the code is burnt and a new
    /// session token (hex, 64 chars) is returned.
    ///
    /// # Errors
    /// [`RedeemError::Locked`] while the source is locked (the attempt is not
    /// evaluated), else [`RedeemError::Invalid`] for a wrong, expired or used code.
    pub fn redeem(&self, code: &str, peer: IpAddr) -> Result<String, RedeemError> {
        let now = (self.clock)();
        let bucket_key = source_bucket(peer);
        let mut inner = self.lock();
        if let Some(bucket) = inner.buckets.get_mut(&bucket_key) {
            bucket.last_seen = now;
            if let Some(until) = bucket.locked_until {
                if now < until {
                    return Err(RedeemError::Locked(until - now));
                }
                bucket.locked_until = None;
                bucket.failures = 0;
                bucket.window_start = now;
            }
        }
        let presented = digest(&normalize(code));
        let ok = inner.code.as_ref().is_some_and(|stored| {
            bool::from(stored.digest.ct_eq(&presented)) && now < stored.expires
        });
        if !ok {
            Self::record_failure(&mut inner, bucket_key, now);
            return Err(RedeemError::Invalid);
        }
        inner.code = None;
        inner.buckets.remove(&bucket_key);
        let mut token = [0u8; 32];
        random_bytes(&mut token);
        let key = digest(&hex(&token));
        inner.sessions.retain(|_, s| now < s.expires);
        if inner.sessions.len() >= SESSION_CAPACITY {
            if let Some(oldest) = inner
                .sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| *k)
            {
                inner.sessions.remove(&oldest);
            }
        }
        inner.sessions.insert(
            key,
            Session {
                expires: now + SESSION_TTL,
            },
        );
        Ok(hex(&token))
    }

    fn record_failure(inner: &mut Inner, key: IpAddr, now: std::time::Instant) {
        if !inner.buckets.contains_key(&key) && inner.buckets.len() >= LIMITER_CAPACITY {
            // Evict the least recently seen source that is not locked; if every
            // tracked source is locked, evict the least recently seen anyway.
            let victim = inner
                .buckets
                .iter()
                .filter(|(_, b)| b.locked_until.is_none_or(|u| now >= u))
                .min_by_key(|(_, b)| b.last_seen)
                .or_else(|| inner.buckets.iter().min_by_key(|(_, b)| b.last_seen))
                .map(|(k, _)| *k);
            if let Some(victim) = victim {
                inner.buckets.remove(&victim);
            }
        }
        let bucket = inner.buckets.entry(key).or_insert(Bucket {
            failures: 0,
            window_start: now,
            locked_until: None,
            last_seen: now,
        });
        if now.duration_since(bucket.window_start) >= FAILURE_WINDOW {
            bucket.failures = 0;
            bucket.window_start = now;
        }
        bucket.failures = bucket.failures.saturating_add(1);
        bucket.last_seen = now;
        if bucket.failures >= MAX_FAILURES {
            bucket.locked_until = Some(now + LOCKOUT);
        }
    }

    /// Whether `token` names a live session.
    #[must_use]
    pub fn session_valid(&self, token: &str) -> bool {
        let now = (self.clock)();
        let presented = digest(token);
        let inner = self.lock();
        // HashMap lookup on a SHA-256 of the token: the map key is not the
        // secret, so the lookup leaks nothing useful about other tokens.
        inner
            .sessions
            .get(&presented)
            .is_some_and(|s| now < s.expires)
    }

    /// Number of tracked limiter sources (tests: the bound).
    #[must_use]
    pub fn tracked_sources(&self) -> usize {
        self.lock().buckets.len()
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clocked() -> (SetupAuth, Arc<AtomicU64>) {
        let base = std::time::Instant::now();
        let offset = Arc::new(AtomicU64::new(0));
        let o = offset.clone();
        let auth = SetupAuth::with_clock(Arc::new(move || {
            base + Duration::from_secs(o.load(Ordering::SeqCst))
        }));
        (auth, offset)
    }

    const PEER: IpAddr = IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 1, 0, 0, 0, 7));

    #[test]
    fn word_list_is_bip39_english() {
        let list = word_list();
        assert_eq!(list.len(), 2048);
        assert_eq!(list.first(), Some(&"abandon"));
        assert_eq!(list.last(), Some(&"zoo"));
        let unique: std::collections::HashSet<_> = list.iter().collect();
        assert_eq!(unique.len(), 2048);
    }

    #[test]
    fn mint_gives_eight_list_words_and_redeem_is_single_use() {
        let (auth, _) = clocked();
        let code = auth.mint();
        let parts: Vec<&str> = code.split(' ').collect();
        assert_eq!(parts.len(), CODE_WORDS);
        assert!(parts.iter().all(|w| word_list().contains(w)));
        let typed = code.to_uppercase().replace(' ', "-");
        let token = auth.redeem(&typed, PEER).unwrap();
        assert_eq!(token.len(), 64);
        assert!(auth.session_valid(&token));
        assert_eq!(auth.redeem(&code, PEER), Err(RedeemError::Invalid));
    }

    #[test]
    fn code_and_session_expire_after_an_hour() {
        let (auth, clock) = clocked();
        let code = auth.mint();
        clock.store(3600, Ordering::SeqCst);
        assert_eq!(auth.redeem(&code, PEER), Err(RedeemError::Invalid));
        let code = auth.mint();
        let token = auth.redeem(&code, PEER).unwrap();
        clock.store(3600 + 3599, Ordering::SeqCst);
        assert!(auth.session_valid(&token));
        clock.store(3600 + 3600, Ordering::SeqCst);
        assert!(!auth.session_valid(&token));
    }

    #[test]
    fn five_failures_lock_the_source_for_fifteen_minutes() {
        let (auth, clock) = clocked();
        let code = auth.mint();
        for _ in 0..MAX_FAILURES {
            assert_eq!(auth.redeem("wrong code", PEER), Err(RedeemError::Invalid));
        }
        // Even the right code is refused while locked.
        assert!(matches!(
            auth.redeem(&code, PEER),
            Err(RedeemError::Locked(_))
        ));
        clock.store(LOCKOUT.as_secs() - 1, Ordering::SeqCst);
        assert!(matches!(
            auth.redeem(&code, PEER),
            Err(RedeemError::Locked(_))
        ));
        clock.store(LOCKOUT.as_secs(), Ordering::SeqCst);
        assert!(auth.redeem(&code, PEER).is_ok());
    }

    #[test]
    fn failures_outside_the_window_do_not_accumulate() {
        let (auth, clock) = clocked();
        let code = auth.mint();
        for _ in 0..MAX_FAILURES - 1 {
            let _ = auth.redeem("nope", PEER);
        }
        clock.store(FAILURE_WINDOW.as_secs(), Ordering::SeqCst);
        let _ = auth.redeem("nope", PEER);
        assert!(auth.redeem(&code, PEER).is_ok());
    }

    #[test]
    fn ipv6_sources_share_a_slash_64() {
        let (auth, _) = clocked();
        let code = auth.mint();
        for host in 1..=MAX_FAILURES {
            let peer = IpAddr::V6(std::net::Ipv6Addr::new(
                0x2001,
                0xdb8,
                0,
                1,
                u16::try_from(host).unwrap(),
                0,
                0,
                1,
            ));
            let _ = auth.redeem("nope", peer);
        }
        assert!(matches!(
            auth.redeem(&code, PEER),
            Err(RedeemError::Locked(_))
        ));
        // A different /64 is unaffected.
        let other = IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 2, 0, 0, 0, 1));
        assert!(auth.redeem(&code, other).is_ok());
        assert_eq!(
            source_bucket("::ffff:192.0.2.1".parse().unwrap()),
            "192.0.2.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn limiter_table_is_bounded() {
        let (auth, _) = clocked();
        let _ = auth.mint();
        for i in 0..(LIMITER_CAPACITY + 100) {
            let n = u32::try_from(i).unwrap();
            let peer = IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + n));
            let _ = auth.redeem("nope", peer);
        }
        assert_eq!(auth.tracked_sources(), LIMITER_CAPACITY);
    }

    #[test]
    fn minting_replaces_the_previous_code() {
        let (auth, _) = clocked();
        let first = auth.mint();
        let second = auth.mint();
        assert_eq!(auth.redeem(&first, PEER), Err(RedeemError::Invalid));
        assert!(auth.redeem(&second, PEER).is_ok());
        assert!(!format!("{auth:?}").contains(&second));
    }
}
