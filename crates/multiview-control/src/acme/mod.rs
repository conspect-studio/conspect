//! ACME issuance engine for the network front door (ADR-0107 §5a, #291).
//!
//! The engine runs on its own OS thread with its own current-thread Tokio
//! runtime ([`AcmeService`]); nothing on the output-clock path waits for it
//! (invariants #1/#10). Its only product is an installed certificate in the
//! shared [`crate::certificate_source::AcmeCertificateSource`].
//!
//! * [`egress`]: SSRF-screened HTTPS client for the CA and the DNS API.
//! * [`dns`]: the four-call DNS-01 provider interface.
//! * [`cloudflare`]: the Cloudflare provider (token or global key).
//! * [`store`]: the private 0700 state directory (account, certificate, TXT ledger).
//! * [`engine`]: one issuance attempt, renewal scheduling, alarms, the service thread.
//!
//! No error, log line or `Debug` impl in this module carries a credential,
//! EAB HMAC, account key or private key: failures cross this boundary only as
//! an [`AcmeError`] category.

pub mod cloudflare;
pub mod dns;
pub mod egress;
pub mod engine;
pub mod store;

pub use dns::{DnsFuture, DnsProvider, TxtHandle};
pub use egress::{AcmeHttp, EgressPolicy};
pub use engine::{
    alarm_for, ari_renewal_at, backoff_delay, fallback_renewal_at, AcmeCommand, AcmeEngine,
    AcmeHandle, AcmeService, AlarmHook, Issued, SendError,
};

/// A credential-safe failure category. Underlying HTTP / ACME / DNS errors
/// never escape this boundary with their text (they may echo request data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AcmeError {
    /// A DNS-provider credential is missing, unreadable or refused (401/403,
    /// an inactive token, or a zone it cannot see).
    #[error("DNS provider credential missing, refused or lacking zone permissions")]
    Credential,
    /// The DNS provider API failed (429, 5xx, malformed answer).
    #[error("DNS provider API failed")]
    DnsApi,
    /// The TXT record did not reach every authoritative nameserver in time.
    #[error("DNS-01 record did not propagate to the authoritative nameservers in time")]
    Propagation,
    /// The ACME authority could not be reached or answered with an error.
    #[error("ACME authority unavailable or refused the request")]
    Authority,
    /// The authority rejected a challenge, or the order became invalid.
    #[error("ACME challenge validation failed")]
    Challenge,
    /// The private state directory or certificate install failed.
    #[error("private ACME state could not be read or persisted")]
    Storage,
    /// The EAB credential was missing or malformed.
    #[error("EAB credential unavailable or malformed")]
    Eab,
    /// The egress screen refused a destination (private/link-local address,
    /// plain HTTP, too many redirects, oversize body) or TLS trust failed.
    #[error("outbound connection refused by the egress policy or TLS trust")]
    Egress,
    /// The authority offers none of the configured challenges.
    #[error("the ACME authority offers no supported challenge")]
    Unsupported,
    /// The issued certificate does not match the request (SAN set, key).
    #[error("issued certificate does not match the request")]
    Mismatch,
    /// The whole attempt exceeded its deadline.
    #[error("ACME attempt timed out")]
    Timeout,
    /// The configuration cannot drive an issuance (e.g. DNS-01 without a provider).
    #[error("ACME configuration incomplete")]
    Config,
}

impl AcmeError {
    /// The sanitized token reported in `CertificateStatus.last_error`.
    #[must_use]
    pub const fn as_reason(self) -> &'static str {
        match self {
            Self::Credential => "dns-credential-rejected",
            Self::DnsApi => "dns-api-failed",
            Self::Propagation => "dns-propagation-timeout",
            Self::Authority => "ca-unavailable",
            Self::Challenge => "challenge-failed",
            Self::Storage => "state-storage-failed",
            Self::Eab => "eab-unavailable",
            Self::Egress => "egress-refused",
            Self::Unsupported => "ca-offers-no-supported-challenge",
            Self::Mismatch => "certificate-mismatch",
            Self::Timeout => "attempt-timed-out",
            Self::Config => "acme-config-incomplete",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_distinct_tokens() {
        let all = [
            AcmeError::Credential,
            AcmeError::DnsApi,
            AcmeError::Propagation,
            AcmeError::Authority,
            AcmeError::Challenge,
            AcmeError::Storage,
            AcmeError::Eab,
            AcmeError::Egress,
            AcmeError::Unsupported,
            AcmeError::Mismatch,
            AcmeError::Timeout,
            AcmeError::Config,
        ];
        let set: std::collections::HashSet<_> = all.iter().map(|e| e.as_reason()).collect();
        assert_eq!(set.len(), all.len());
        assert_eq!(
            AcmeError::Unsupported.as_reason(),
            "ca-offers-no-supported-challenge"
        );
        for e in all {
            assert!(e
                .as_reason()
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-'));
        }
    }
}
