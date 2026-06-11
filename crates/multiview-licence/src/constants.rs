//! The hard constants of the entitlement plane (ADR-0050 §2 / §4, brief §2).
//!
//! These are load-bearing across the machine **and** the external portal
//! prototypes. They are recorded here as the single in-repo source of truth so
//! the implementation, the tests, and the portals agree byte-for-byte. **Do not
//! round, re-derive, or "tidy" any value** — a portal that shows "35 days" and a
//! machine that enforces 30 is a support incident (ADR-0050 §4). The property
//! tests assert these exactly.

/// `LEASE_FULL` — a successful **online** heartbeat grants a 35-day entitlement
/// lease (covers a missed monthly contact + margin). Brief §2.2.
pub const LEASE_FULL_DAYS: i64 = 35;

/// `LEASE_GRACE` — after lease expiry, a 14-day grace period where conveniences
/// still work but warnings escalate. Brief §2.2.
pub const LEASE_GRACE_DAYS: i64 = 14;

/// `LEASE_HARD` — the absolute outer bound from last contact before the hardest
/// rung applies; also the term granted to an **offline** (file/relay) lease.
/// Brief §2.2.
pub const LEASE_HARD_DAYS: i64 = 90;

/// `ACTIVATION_WINDOW` — the rolling window inside which an entitlement must see
/// at least the minimum (monthly) heartbeat contact. Brief §2.1.
pub const ACTIVATION_WINDOW_DAYS: i64 = 31;

/// `CLAIM_CODE_LEN` — pairing/claim codes are exactly 6 characters from an
/// ambiguity-free alphabet (brief §2.4). The exact glyph set is an
/// operator-confirm item (brief O3); only the length is pinned here.
pub const CLAIM_CODE_LEN: usize = 6;

/// The number of whole days past **online** lease expiry at which the ladder
/// leaves grace and enters the soft-lapse rung (blocks new instances, as data).
/// Soft lapse spans `(LEASE_GRACE_DAYS, LAPSED_SOFT_MAX_DAYS]` past expiry.
pub const LAPSED_SOFT_MAX_DAYS: i64 = 45;

/// The total length of the evaluation period, in days (brief §6 evaluation
/// track). Within it the canvas is clean until [`EVALUATION_WATERMARK_DAY`].
pub const EVALUATION_PERIOD_DAYS: i64 = 60;

/// The evaluation day from which an honest watermark is stamped (the canvas is
/// clean for the first 30 days; the watermark engages on day 31).
pub const EVALUATION_WATERMARK_DAY: i64 = 31;

/// `FINGERPRINT_MATCH_STRONG` — a perfect match of the salted hardware-fingerprint
/// component set scores 100 (brief §2.3).
pub const FINGERPRINT_MATCH_STRONG: u8 = 100;

/// `FINGERPRINT_MATCH_THRESHOLD` — at/above 70 the machine is treated as the
/// *same* machine (hardware drift tolerated); below 70 it is a *new* machine
/// requiring re-claim (brief §2.3).
pub const FINGERPRINT_MATCH_THRESHOLD: u8 = 70;
