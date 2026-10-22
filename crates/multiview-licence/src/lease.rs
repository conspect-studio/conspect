//! The heartbeat-granted entitlement **lease** (ADR-0050 §4, brief §2.2, §6.1,
//! §12).
//!
//! A lease is the dated grant a successful heartbeat produces. It is pure data:
//! all of its bounds are computed from the grant instant and the exact day
//! constants ([`crate::constants`]) using `chrono::Duration` — **never float**
//! (CLAUDE.md safety rule #6). The lease arithmetic is the input to the
//! enforcement ladder ([`crate::ladder`]); this module only *holds* and *derives*
//! the dated bounds, it does not enforce anything.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::constants::{LEASE_FULL_DAYS, LEASE_GRACE_DAYS, LEASE_HARD_DAYS};

/// Where a lease grant came from. Internally tagged on `source` so it is robust
/// across TOML and JSON (conventions §5 — **never** `#[serde(untagged)]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LeaseSource {
    /// Granted by a direct heartbeat to the licence server (35-day term).
    Online,
    /// Granted via an end-to-end-signed mesh relay (brief §9.2) — still a fresh
    /// online-equivalent grant, distinguished for audit.
    Relay,
    /// Provisioned from a signed offline lease file (90-day hard term, brief
    /// §2.2): a machine with no internet path.
    File,
}

impl LeaseSource {
    /// The lease term, in days, granted for this source. Online/relay grants get
    /// [`LEASE_FULL_DAYS`]; an offline file lease gets the [`LEASE_HARD_DAYS`]
    /// outer bound up front (brief §2.2).
    #[must_use]
    pub const fn term_days(self) -> i64 {
        match self {
            LeaseSource::Online | LeaseSource::Relay => LEASE_FULL_DAYS,
            LeaseSource::File => LEASE_HARD_DAYS,
        }
    }

    /// The heartbeat **transport** label this grant arrived over, as the
    /// heartbeat-status surface reports it (brief §7/§11): an `Online` grant came
    /// over a `"direct"` licence-server contact, a `Relay` grant over a mesh
    /// `"relay"`, and a `File` grant from a dropped offline lease `"file"`. A
    /// stable lowercase slug the API + portal share.
    #[must_use]
    pub const fn transport(self) -> &'static str {
        match self {
            LeaseSource::Online => "direct",
            LeaseSource::Relay => "relay",
            LeaseSource::File => "file",
        }
    }
}

/// A dated entitlement lease. All bounds are derived from `granted_at` + the
/// exact day constants; this type carries them explicitly so the API, the UI,
/// and the portals render the same dates without re-deriving (ADR-0050 §4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Lease {
    /// The opaque lease serial (server-issued; identifies this grant). Not a
    /// hardware identifier (brief §8 — data minimisation).
    pub serial: String,
    /// Where this grant came from (drives the term).
    pub source: LeaseSource,
    /// When the lease was granted (the successful heartbeat instant).
    pub granted_at: DateTime<Utc>,
    /// When the lease term expires (`granted_at` + the source term).
    pub expires_at: DateTime<Utc>,
    /// The number of grace days that follow expiry (`LEASE_GRACE`).
    pub grace_days: i64,
    /// The end of the grace window (`expires_at` + `grace_days`).
    pub grace_until: DateTime<Utc>,
    /// The absolute hard bound from grant (`granted_at` + `LEASE_HARD`); past
    /// this the hardest (data-only) rung applies.
    pub hard_at: DateTime<Utc>,
    /// When the next heartbeat is due to stay comfortably inside the activation
    /// window (`granted_at` + the activation window).
    pub next_contact_due: DateTime<Utc>,
}

impl Lease {
    /// Build an **online/relay** lease (35-day term) granted at `granted_at`.
    ///
    /// `activation_window_days` is normally [`crate::ACTIVATION_WINDOW_DAYS`];
    /// it is a parameter so the caller (the cli, fed by the licence server) owns
    /// the policy value rather than this leaf crate baking a clock read.
    #[must_use]
    pub fn new_full(
        serial: String,
        granted_at: DateTime<Utc>,
        source: LeaseSource,
        activation_window_days: i64,
    ) -> Self {
        Self::new(serial, granted_at, source, activation_window_days)
    }

    /// Build an **online/relay** lease whose `expires_at` is anchored to the
    /// cryptographically-signed `not_after` (epoch ms) rather than to a local
    /// clock read — the lease the heartbeat installs MUST expire exactly when the
    /// signed lease says, so a short-lived or replayed-old signed lease can never
    /// become a fresh 35-day term (CONSPECT-3 / ADR-0096, ADR-I006).
    ///
    /// `granted_at` is back-derived as `expires_at − term`, so the whole ladder
    /// (grace, hard bound, next-contact) is consistent with the signed window.
    /// Returns `None` when `not_after_ms` is at or before `now_ms` — a
    /// signed-but-expired (or replayed) lease is rejected, never installed.
    #[must_use]
    pub fn new_online_expiring_at(
        serial: String,
        not_after_ms: i64,
        now_ms: i64,
        activation_window_days: i64,
    ) -> Option<Self> {
        if not_after_ms <= now_ms {
            return None;
        }
        let expires_at = DateTime::<Utc>::from_timestamp_millis(not_after_ms)?;
        let source = LeaseSource::Online;
        let granted_at = expires_at - Duration::days(source.term_days());
        let grace_until = expires_at + Duration::days(LEASE_GRACE_DAYS);
        let hard_at = granted_at + Duration::days(LEASE_HARD_DAYS);
        let next_contact_due = granted_at + Duration::days(activation_window_days);
        Some(Self {
            serial,
            source,
            granted_at,
            expires_at,
            grace_days: LEASE_GRACE_DAYS,
            grace_until,
            hard_at,
            next_contact_due,
        })
    }

    /// Build an **offline** (file) lease with the 90-day hard term.
    #[must_use]
    pub fn new_offline(
        serial: String,
        granted_at: DateTime<Utc>,
        activation_window_days: i64,
    ) -> Self {
        Self::new(
            serial,
            granted_at,
            LeaseSource::File,
            activation_window_days,
        )
    }

    fn new(
        serial: String,
        granted_at: DateTime<Utc>,
        source: LeaseSource,
        activation_window_days: i64,
    ) -> Self {
        let expires_at = granted_at + Duration::days(source.term_days());
        let grace_until = expires_at + Duration::days(LEASE_GRACE_DAYS);
        let hard_at = granted_at + Duration::days(LEASE_HARD_DAYS);
        let next_contact_due = granted_at + Duration::days(activation_window_days);
        Self {
            serial,
            source,
            granted_at,
            expires_at,
            grace_days: LEASE_GRACE_DAYS,
            grace_until,
            hard_at,
            next_contact_due,
        }
    }

    /// Whole days `now` is past `expires_at` (negative/zero before expiry).
    ///
    /// Truncating-toward-zero day arithmetic on the exact instant difference;
    /// the ladder boundaries are defined in whole days past expiry.
    #[must_use]
    pub fn days_past_expiry(&self, now: DateTime<Utc>) -> i64 {
        (now - self.expires_at).num_days()
    }

    /// The lease term expiry as an RFC 3339 string (`valid_to` on the install
    /// response). The crate owns the `chrono` arithmetic, so date formatting is
    /// rendered here once rather than re-derived by every caller.
    #[must_use]
    pub fn valid_to_rfc3339(&self) -> String {
        self.expires_at.to_rfc3339()
    }
}
