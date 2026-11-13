//! The DNS-01 provider interface (ADR-0107 §5a): exactly four calls.
//!
//! The ACME core knows nothing provider-specific. `remove_txt` is called for
//! every published record on every path (success, failure, timeout), and the
//! engine's pending-TXT ledger repeats it after a crash.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::AcmeError;

/// A boxed provider operation, so the trait stays object safe.
pub type DnsFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AcmeError>> + Send + 'a>>;

/// Opaque handle to one published TXT record. Serializable so the engine can
/// persist it in the pending-TXT ledger; the provider owns its format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TxtHandle(pub String);

/// A DNS-01 provider.
pub trait DnsProvider: Send + Sync {
    /// Stable provider name stored in the ledger (e.g. `"cloudflare"`).
    fn name(&self) -> &'static str;

    /// Check the credential and that every name's zone is reachable, before
    /// any order. Wildcard prefixes are the provider's to strip.
    fn validate_credential<'a>(&'a self, names: &'a [String]) -> DnsFuture<'a, ()>;

    /// Publish one TXT value at `fqdn` (several values on one name must coexist).
    fn publish_txt<'a>(&'a self, fqdn: &'a str, value: &'a str) -> DnsFuture<'a, TxtHandle>;

    /// Wait until every authoritative nameserver of the zone serves `value`
    /// at `fqdn`, or fail with [`AcmeError::Propagation`] after `deadline`.
    fn confirm_at_authoritative<'a>(
        &'a self,
        fqdn: &'a str,
        value: &'a str,
        deadline: Duration,
    ) -> DnsFuture<'a, ()>;

    /// A short, credential-free human detail for the last
    /// [`AcmeError::Propagation`] (how confirmation was attempted). `None`
    /// when the provider has nothing to add.
    fn propagation_detail(&self) -> Option<String> {
        None
    }

    /// Remove a record this provider published. An already-absent record is success.
    fn remove_txt<'a>(&'a self, handle: &'a TxtHandle) -> DnsFuture<'a, ()>;
}

/// The `_acme-challenge` name for an identifier (wildcard prefix stripped).
#[must_use]
pub fn challenge_fqdn(name: &str) -> String {
    format!(
        "_acme-challenge.{}",
        name.strip_prefix("*.")
            .unwrap_or(name)
            .trim_end_matches('.')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_name_strips_wildcard() {
        assert_eq!(challenge_fqdn("*.w.example"), "_acme-challenge.w.example");
        assert_eq!(challenge_fqdn("w.example."), "_acme-challenge.w.example");
    }

    #[test]
    fn provider_is_object_safe_and_handle_serializes_as_string() {
        fn _object_safe(_: &dyn DnsProvider) {}
        let json = serde_json::to_string(&TxtHandle("z/r".into())).unwrap_or_default();
        assert_eq!(json, "\"z/r\"");
    }
}
