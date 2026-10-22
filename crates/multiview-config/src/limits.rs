//! Management-plane request-concurrency + rate limits (SEC-14) — the control-plane
//! `DoS` floor, as operator configuration.
//!
//! [`ManagementLimits`] is the `[control.limits]` config section: the
//! concurrent-request cap and the per-IP (pre-auth) / per-API-key (post-auth)
//! token-bucket rates the `multiview-control` middleware enforces. Absent ⇒ the
//! secure defaults below (limits **on**). The runtime limiter lives in
//! `multiview-control`; this crate only models + validates the knobs.
//!
//! The rate + concurrency caps engage after a request's headers are parsed, so they
//! bound in-flight requests (and the long-lived WS/SSE sessions that hold a
//! concurrency permit for their lifetime) — **not** idle keep-alive connections,
//! which hold no permit. A half-open, slow-header connection (slowloris) is bounded
//! two ways: [`ManagementLimits::header_read_timeout_secs`] bounds its *lifetime* (the
//! serve loop drops it if the header block does not arrive in time), and
//! [`ManagementLimits::max_connections`] / [`ManagementLimits::max_connections_per_ip`]
//! bound the *population* the serve loop will accept at all — the floor the
//! request-level caps miss, since a connection stalled before its headers parse never
//! takes a request permit (SEC-14 / #126; see
//! [ADR-W028](../../../docs/decisions/ADR-W028.md),
//! [ADR-W031](../../../docs/decisions/ADR-W031.md)).

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// The largest concurrent-request cap the runtime can honour: the
/// `tokio::sync::Semaphore` `MAX_PERMITS` ceiling (`usize::MAX >> 3`). A configured
/// value above this cannot be installed, so [`ManagementLimits::validate`] rejects
/// it **fail-closed** at config load rather than let the runtime silently clamp it
/// to a different effective cap. `multiview-control` carries a static assertion that
/// this equals `tokio::sync::Semaphore::MAX_PERMITS`, so the two can never drift.
pub const MAX_CONCURRENT_REQUESTS_CEILING: usize = usize::MAX >> 3;

/// Default concurrent in-flight request cap: generous for a handful of operators
/// plus the SPA and its realtime stream, while bounding a request flood.
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 256;
/// Default per-IP burst — covers the SPA's initial fan-out of resource fetches on
/// load without throttling a legitimate operator.
const DEFAULT_PER_IP_BURST: u32 = 120;
/// Default per-IP steady-state rate (requests/second), pre-auth.
const DEFAULT_PER_IP_REFILL_PER_SEC: u32 = 40;
/// Default per-API-key burst — more generous than per-IP: an authenticated
/// client is trusted further.
const DEFAULT_PER_API_KEY_BURST: u32 = 240;
/// Default per-API-key steady-state rate (requests/second), post-auth.
const DEFAULT_PER_API_KEY_REFILL_PER_SEC: u32 = 80;
/// Default header-read timeout (seconds): the maximum time the serve loop waits to
/// read a request's full header block before dropping the connection. Generous for
/// any legitimate client (headers arrive in milliseconds) while bounding a
/// slow-header slowloris.
const DEFAULT_HEADER_READ_TIMEOUT_SECS: u64 = 20;
/// Default cap on concurrently-accepted connections across all peers: bounds the
/// socket + task population a connection flood can pin **before** any request parses
/// (the slowloris / slow-TLS-handshake floor the request-level caps miss). Generous
/// for a handful of operators plus the SPA's fan-out and its long-lived WS/SSE
/// sessions, while bounding a flood.
const DEFAULT_MAX_CONNECTIONS: usize = 1024;
/// Default cap on concurrently-accepted connections from a single peer IP: bounds a
/// single-source flood while leaving headroom for one host running the SPA (resource
/// fan-out + WS + SSE). Must not exceed [`DEFAULT_MAX_CONNECTIONS`].
const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 256;

fn default_enabled() -> bool {
    true
}

fn default_max_concurrent_requests() -> usize {
    DEFAULT_MAX_CONCURRENT_REQUESTS
}

fn default_header_read_timeout_secs() -> u64 {
    DEFAULT_HEADER_READ_TIMEOUT_SECS
}

fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}

fn default_max_connections_per_ip() -> usize {
    DEFAULT_MAX_CONNECTIONS_PER_IP
}

fn default_per_ip() -> RateLimitConfig {
    RateLimitConfig {
        burst: DEFAULT_PER_IP_BURST,
        refill_per_sec: DEFAULT_PER_IP_REFILL_PER_SEC,
    }
}

fn default_per_api_key() -> RateLimitConfig {
    RateLimitConfig {
        burst: DEFAULT_PER_API_KEY_BURST,
        refill_per_sec: DEFAULT_PER_API_KEY_REFILL_PER_SEC,
    }
}

/// Field-level default for a partially specified [`RateLimitConfig`] sub-table: the
/// conservative per-IP baseline. A field-level serde default is context-free — it
/// cannot tell `per_ip` from `per_api_key` — so an unset `burst` / `refill_per_sec`
/// in a partial bucket falls back to this single stricter baseline: a partial bucket
/// errs strict, never looser than intended for a `DoS` floor. Name both fields of a
/// bucket to set it exactly.
fn default_rate_burst() -> u32 {
    DEFAULT_PER_IP_BURST
}

fn default_rate_refill_per_sec() -> u32 {
    DEFAULT_PER_IP_REFILL_PER_SEC
}

/// A token-bucket rate: a `burst` ceiling replenished at `refill_per_sec`
/// requests per second. Over-budget requests get `429` + `Retry-After`.
///
/// Each field carries its own serde default (the conservative per-IP baseline), so a
/// partially specified sub-table inherits any unset field rather than failing to
/// deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RateLimitConfig {
    /// The maximum number of requests admitted in an instantaneous burst. Unset in a
    /// partial sub-table ⇒ the conservative per-IP baseline ([`default_rate_burst`]).
    #[serde(default = "default_rate_burst")]
    pub burst: u32,
    /// The steady-state replenishment rate, in requests per second. Unset in a partial
    /// sub-table ⇒ the conservative per-IP baseline ([`default_rate_refill_per_sec`]).
    #[serde(default = "default_rate_refill_per_sec")]
    pub refill_per_sec: u32,
}

impl RateLimitConfig {
    /// Reject a nonsensical rate.
    ///
    /// # Errors
    /// [`ConfigError::Validation`] if `burst` or `refill_per_sec` is zero — a zero
    /// burst would reject every request forever, and a zero refill would never
    /// replenish the bucket. `which` names the section for the message.
    fn validate(&self, which: &str) -> Result<(), ConfigError> {
        if self.burst == 0 {
            return Err(ConfigError::Validation(format!(
                "control.limits.{which}.burst must be >= 1 (0 would reject every request)"
            )));
        }
        if self.refill_per_sec == 0 {
            return Err(ConfigError::Validation(format!(
                "control.limits.{which}.refill_per_sec must be >= 1 (0 would never replenish \
                 the bucket)"
            )));
        }
        Ok(())
    }
}

/// The `[control.limits]` section: the management-plane request-concurrency + rate
/// caps (SEC-14 control-plane `DoS` floor).
///
/// Absent ⇒ the secure defaults (limits enabled). Every field defaults
/// individually — including `burst` / `refill_per_sec` inside the `per_ip` and
/// `per_api_key` sub-tables — so a partial section OR a partial sub-table overrides
/// only the knobs it names; an unset rate field in a partial sub-table falls back to
/// the conservative per-IP baseline (erring strict). Name both fields of a bucket to
/// set that bucket exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ManagementLimits {
    /// Whether the limits are enforced at all. Default `true` (secure default);
    /// set `false` only for a fully-trusted single-tenant deployment.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The cap on concurrent in-flight requests. Over-cap requests get `503` +
    /// `Retry-After` rather than queueing. Default `256`.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// The per-source-IP token bucket, applied **before** authentication to
    /// protect the auth path from brute-force. Keyed on the peer IP.
    #[serde(default = "default_per_ip")]
    pub per_ip: RateLimitConfig,
    /// The per-API-key token bucket, applied **after** authentication so one
    /// credential cannot monopolise the management plane. Keyed on the
    /// authenticated key id.
    #[serde(default = "default_per_api_key")]
    pub per_api_key: RateLimitConfig,
    /// The header-read timeout, in **seconds**: the maximum time the serve loop
    /// waits to read a request's full header block before dropping the connection.
    /// Bounds a slow-header ("slowloris") client that dribbles headers to pin a
    /// connection open — the rate + concurrency caps above engage only *after*
    /// headers are parsed, so they do not cover it. Applied to every served
    /// connection **independent of `enabled`** (which gates only the shed layers): a
    /// generous header-read timeout has no downside for a legitimate client, so it
    /// stays on even for a trusted-network deployment. Default `20`.
    #[serde(default = "default_header_read_timeout_secs")]
    pub header_read_timeout_secs: u64,
    /// The cap on concurrently-accepted connections across all peers, enforced by the
    /// control-plane serve loop **at accept** — before any request headers parse. This
    /// is the population bound the request-level caps miss: `max_concurrent_requests`
    /// and the token buckets engage only *after* a request's headers are parsed, so a
    /// flood of half-open, slow-header (or slow-TLS-handshake) connections pins sockets
    /// and tasks without ever taking a request permit. Over-cap connections are dropped
    /// at accept. Enforced only while `enabled` (a shed-layer cap, like
    /// `max_concurrent_requests`). Default `1024`.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// The cap on concurrently-accepted connections from a single peer IP, enforced at
    /// accept **before** authentication so one source cannot monopolise
    /// `max_connections`. Must not exceed `max_connections` (a looser per-IP cap could
    /// never bind). Over-cap connections are dropped at accept. Enforced only while
    /// `enabled`. Default `256`.
    #[serde(default = "default_max_connections_per_ip")]
    pub max_connections_per_ip: usize,
}

impl Default for ManagementLimits {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            max_concurrent_requests: default_max_concurrent_requests(),
            per_ip: default_per_ip(),
            per_api_key: default_per_api_key(),
            header_read_timeout_secs: default_header_read_timeout_secs(),
            max_connections: default_max_connections(),
            max_connections_per_ip: default_max_connections_per_ip(),
        }
    }
}

impl ManagementLimits {
    /// Validate the limits.
    ///
    /// # Errors
    /// [`ConfigError::Validation`] if the concurrency cap is zero or above the runtime
    /// ceiling, the header-read timeout is zero, either connection cap
    /// (`max_connections` / `max_connections_per_ip`) is zero, the global connection
    /// cap is above the runtime ceiling, the per-IP connection cap exceeds the global
    /// one, or either token-bucket rate has a zero `burst` or `refill_per_sec` — each
    /// would turn the `DoS` floor into a self-inflicted outage (or, for a looser per-IP
    /// cap, a floor that never binds). Validated regardless of `enabled` so a typo is
    /// caught even while the limits are temporarily off.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_concurrent_requests == 0 {
            return Err(ConfigError::Validation(
                "control.limits.max_concurrent_requests must be >= 1 (0 would reject every \
                 request)"
                    .to_owned(),
            ));
        }
        if self.max_concurrent_requests > MAX_CONCURRENT_REQUESTS_CEILING {
            return Err(ConfigError::Validation(format!(
                "control.limits.max_concurrent_requests must be <= \
                 {MAX_CONCURRENT_REQUESTS_CEILING} (the runtime Semaphore ceiling); a larger \
                 value cannot be installed and must not be silently clamped to a different cap"
            )));
        }
        if self.header_read_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "control.limits.header_read_timeout_secs must be >= 1 (0 would drop every \
                 connection before it can send its headers)"
                    .to_owned(),
            ));
        }
        if self.max_connections == 0 {
            return Err(ConfigError::Validation(
                "control.limits.max_connections must be >= 1 (0 would drop every connection at \
                 accept)"
                    .to_owned(),
            ));
        }
        if self.max_connections > MAX_CONCURRENT_REQUESTS_CEILING {
            return Err(ConfigError::Validation(format!(
                "control.limits.max_connections must be <= {MAX_CONCURRENT_REQUESTS_CEILING} (the \
                 runtime Semaphore ceiling the accept-level connection cap installs into); a larger \
                 value cannot be installed and must not be silently clamped to a different cap"
            )));
        }
        if self.max_connections_per_ip == 0 {
            return Err(ConfigError::Validation(
                "control.limits.max_connections_per_ip must be >= 1 (0 would drop every \
                 connection at accept)"
                    .to_owned(),
            ));
        }
        if self.max_connections_per_ip > self.max_connections {
            return Err(ConfigError::Validation(format!(
                "control.limits.max_connections_per_ip ({}) must be <= max_connections ({}); a \
                 per-IP cap above the global cap can never bind",
                self.max_connections_per_ip, self.max_connections
            )));
        }
        self.per_ip.validate("per_ip")?;
        self.per_api_key.validate("per_api_key")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{ManagementLimits, RateLimitConfig};

    #[test]
    fn secure_defaults_validate_ok() {
        let limits = ManagementLimits::default();
        assert!(limits.enabled);
        assert_eq!(limits.max_concurrent_requests, 256);
        assert_eq!(limits.per_ip.burst, 120);
        assert_eq!(limits.per_ip.refill_per_sec, 40);
        assert_eq!(limits.per_api_key.burst, 240);
        assert_eq!(limits.per_api_key.refill_per_sec, 80);
        assert_eq!(limits.header_read_timeout_secs, 20);
        assert_eq!(limits.max_connections, 1024);
        assert_eq!(limits.max_connections_per_ip, 256);
        limits
            .validate()
            .expect("the secure defaults must validate");
    }

    #[test]
    fn a_zero_header_read_timeout_is_rejected() {
        let limits = ManagementLimits {
            header_read_timeout_secs: 0,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero header-read timeout would drop every connection before it can send its \
             headers and must fail config load"
        );
    }

    #[test]
    fn a_zero_concurrency_cap_is_rejected() {
        let limits = ManagementLimits {
            max_concurrent_requests: 0,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero concurrency cap would reject every request and must fail config load"
        );
    }

    #[test]
    fn a_zero_max_connections_is_rejected() {
        let limits = ManagementLimits {
            max_connections: 0,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero connection cap would drop every connection at accept and must fail config load"
        );
    }

    #[test]
    fn a_zero_max_connections_per_ip_is_rejected() {
        let limits = ManagementLimits {
            max_connections_per_ip: 0,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero per-IP connection cap would drop every connection at accept and must fail \
             config load"
        );
    }

    #[test]
    fn a_per_ip_connection_cap_above_the_global_cap_is_rejected() {
        // A per-IP cap looser than the global cap is a misconfiguration: the per-IP
        // limit could never bind (the global cap engages first), so a single source
        // could still reach the global cap. Err strict — catch the typo at load.
        let limits = ManagementLimits {
            max_connections: 100,
            max_connections_per_ip: 101,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a per-IP connection cap above the global cap must fail config load"
        );
    }

    #[test]
    fn a_max_connections_above_the_runtime_ceiling_is_rejected() {
        // The runtime installs `max_connections` into a `tokio::sync::Semaphore`
        // (the accept-level ConnectionAdmission global cap), whose `MAX_PERMITS`
        // ceiling is `usize::MAX >> 3` — the SAME bound `max_concurrent_requests`
        // hits. A config value above it cannot be honoured, so fail closed at load
        // rather than let the runtime silently clamp to a different effective cap
        // (mirrors the round-1 F4 rule for the request-concurrency cap).
        let runtime_ceiling = usize::MAX >> 3;
        let limits = ManagementLimits {
            max_connections: runtime_ceiling + 1,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a connection cap above the runtime Semaphore ceiling must fail config load, not be \
             silently clamped to a different value"
        );
    }

    #[test]
    fn the_max_connections_runtime_ceiling_itself_validates() {
        // The boundary value (exactly the runtime ceiling) is honourable, so it must
        // pass — only strictly-larger values are rejected. `max_connections_per_ip`
        // is pinned to the ceiling too so the per-IP <= global invariant holds.
        let runtime_ceiling = usize::MAX >> 3;
        let limits = ManagementLimits {
            max_connections: runtime_ceiling,
            max_connections_per_ip: runtime_ceiling,
            ..ManagementLimits::default()
        };
        limits.validate().expect(
            "a connection cap equal to the runtime ceiling is honourable and must validate",
        );
    }

    #[test]
    fn a_per_ip_connection_cap_equal_to_the_global_cap_validates() {
        // The boundary (per-IP == global) is honourable — a single trusted source may
        // use the whole budget — so it must pass; only strictly-larger is rejected.
        let limits = ManagementLimits {
            max_connections: 100,
            max_connections_per_ip: 100,
            ..ManagementLimits::default()
        };
        limits
            .validate()
            .expect("a per-IP cap equal to the global cap is honourable and must validate");
    }

    #[test]
    fn a_zero_per_ip_burst_is_rejected() {
        let limits = ManagementLimits {
            per_ip: RateLimitConfig {
                burst: 0,
                refill_per_sec: 40,
            },
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero per-IP burst would reject every request and must fail config load"
        );
    }

    #[test]
    fn a_zero_per_api_key_refill_is_rejected() {
        let limits = ManagementLimits {
            per_api_key: RateLimitConfig {
                burst: 240,
                refill_per_sec: 0,
            },
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a zero per-API-key refill would never replenish and must fail config load"
        );
    }

    #[test]
    fn a_concurrency_cap_above_the_runtime_ceiling_is_rejected() {
        // The runtime installs the cap into a `tokio::sync::Semaphore`, whose
        // `MAX_PERMITS` ceiling is `usize::MAX >> 3`. A config value above it cannot
        // be honoured — the old code silently clamped, so a valid-looking config
        // produced a DIFFERENT cap than written. Fail closed at load instead (F4).
        let runtime_ceiling = usize::MAX >> 3;
        let limits = ManagementLimits {
            max_concurrent_requests: runtime_ceiling + 1,
            ..ManagementLimits::default()
        };
        assert!(
            limits.validate().is_err(),
            "a concurrency cap above the runtime Semaphore ceiling must fail config load, \
             not be silently clamped to a different value"
        );
    }

    #[test]
    fn the_runtime_ceiling_itself_validates() {
        // The boundary value (exactly the runtime ceiling) is honourable, so it must
        // pass — only strictly-larger values are rejected.
        let runtime_ceiling = usize::MAX >> 3;
        let limits = ManagementLimits {
            max_concurrent_requests: runtime_ceiling,
            ..ManagementLimits::default()
        };
        limits
            .validate()
            .expect("a cap equal to the runtime ceiling is honourable and must validate");
    }

    #[test]
    fn an_absent_section_deserialises_to_the_secure_defaults() {
        // A `[control.limits]` written with no fields (or omitted entirely) fills
        // every knob from its default — the secure default posture.
        let parsed: ManagementLimits =
            serde_json::from_str("{}").expect("an empty object fills every default");
        assert_eq!(parsed, ManagementLimits::default());
    }

    #[test]
    fn a_partial_section_overrides_only_named_knobs() {
        let parsed: ManagementLimits = serde_json::from_str(r#"{"max_concurrent_requests": 16}"#)
            .expect("a partial section fills the rest from defaults");
        assert_eq!(parsed.max_concurrent_requests, 16);
        // Untouched knobs keep their secure defaults.
        assert_eq!(parsed.per_ip, ManagementLimits::default().per_ip);
        assert!(parsed.enabled);
    }

    #[test]
    fn a_partial_rate_sub_table_inherits_the_unset_fields_default() {
        // A partially specified `per_ip` sub-table names only `burst`; the unset
        // `refill_per_sec` must fall back to its field-level default rather than fail
        // deserialization. The documented "every field defaults individually" contract
        // reaches INTO the rate sub-tables, not just the top level. Before the field
        // defaults were added this errored with `missing field `refill_per_sec``.
        let parsed: ManagementLimits = serde_json::from_str(r#"{"per_ip": {"burst": 10}}"#)
            .expect("a partial per_ip sub-table fills refill_per_sec from its field default");
        assert_eq!(parsed.per_ip.burst, 10);
        // The unset field inherits the canonical RateLimitConfig field default — the
        // stricter per-IP baseline (a partial bucket errs strict).
        assert_eq!(
            parsed.per_ip.refill_per_sec,
            super::DEFAULT_PER_IP_REFILL_PER_SEC
        );

        // Symmetric: naming only `refill_per_sec` in `per_api_key` inherits the default
        // `burst` — again the per-IP baseline, not per_api_key's own 240 (err strict).
        let parsed: ManagementLimits =
            serde_json::from_str(r#"{"per_api_key": {"refill_per_sec": 7}}"#)
                .expect("a partial per_api_key sub-table fills burst from its field default");
        assert_eq!(parsed.per_api_key.refill_per_sec, 7);
        assert_eq!(parsed.per_api_key.burst, super::DEFAULT_PER_IP_BURST);
    }
}
