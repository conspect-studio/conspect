//! Real DNS facts for the network report (feature `catalogue`): the CNAME
//! chain, A and AAAA records with TTL, and the resolver used.
//!
//! The lookups are *diagnostic*: they describe what DNS says. They never decide
//! what is dialled — every address that is connected to is still screened by
//! `net_guard` (through `DialGuard`) in the collector. Each lookup is bounded
//! to [`LOOKUP_TIMEOUT`]; a failure is reported as text, never papered over.

use std::net::IpAddr;
use std::time::Duration;

use hickory_resolver::proto::rr::{RData, Record, RecordType};
use hickory_resolver::TokioResolver;

use super::NetworkDns;
use crate::catalogue::sync::BoxFuture;

/// Bound on each individual lookup (CNAME-following A, then AAAA).
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// The DNS seam: tests substitute a stub, so no real DNS runs in tests.
pub trait DnsLookup: Send + Sync {
    /// Look `host` up (a domain name, not an IP literal).
    fn lookup<'a>(&'a self, host: &'a str) -> BoxFuture<'a, NetworkDns>;
}

/// Fold the answer records of an A and an AAAA lookup into a [`NetworkDns`].
///
/// The chain keeps answer order and drops repeats; TTL is the lowest among the
/// address records (when the answer has none it stays `None`).
#[must_use]
pub fn fold_records(records: &[Record], resolver: Option<String>) -> NetworkDns {
    let mut dns = NetworkDns {
        resolver,
        ..NetworkDns::default()
    };
    for r in records {
        match &r.data {
            RData::CNAME(c) => {
                let name = c.0.to_utf8().trim_end_matches('.').to_ascii_lowercase();
                if !name.is_empty() && !dns.cname.contains(&name) {
                    dns.cname.push(name);
                }
            }
            RData::A(a) => {
                let ip = IpAddr::V4(a.0).to_string();
                if !dns.a.contains(&ip) {
                    dns.a.push(ip);
                }
                dns.ttl = Some(dns.ttl.map_or(r.ttl, |t| t.min(r.ttl)));
            }
            RData::AAAA(a) => {
                let ip = IpAddr::V6(a.0).to_string();
                if !dns.aaaa.contains(&ip) {
                    dns.aaaa.push(ip);
                }
                dns.ttl = Some(dns.ttl.map_or(r.ttl, |t| t.min(r.ttl)));
            }
            _ => {}
        }
    }
    dns
}

/// The production lookup: hickory's stub resolver on the system configuration
/// (`/etc/resolv.conf`), plain UDP/TCP.
pub struct SystemDns {
    resolver: Result<TokioResolver, String>,
    label: String,
}

impl std::fmt::Debug for SystemDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemDns")
            .field("resolver", &self.label)
            .finish_non_exhaustive()
    }
}

impl SystemDns {
    /// Read the system resolver configuration. A failure is kept and reported
    /// on every lookup instead of aborting start-up.
    #[must_use]
    pub fn new() -> Self {
        match hickory_resolver::system_conf::read_system_conf() {
            Ok((config, mut opts)) => {
                let label = config
                    .name_servers()
                    .iter()
                    .map(|n| n.ip.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                opts.timeout = LOOKUP_TIMEOUT;
                opts.attempts = 1;
                let resolver = TokioResolver::builder_with_config(
                    config,
                    hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
                )
                .with_options(opts)
                .build()
                .map_err(|e| format!("resolver: {e}"));
                Self {
                    resolver,
                    label: if label.is_empty() {
                        "system".to_owned()
                    } else {
                        label
                    },
                }
            }
            Err(e) => Self {
                resolver: Err(format!("resolver configuration: {e}")),
                label: String::new(),
            },
        }
    }
}

impl Default for SystemDns {
    fn default() -> Self {
        Self::new()
    }
}

fn clean(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(160).collect()
}

impl DnsLookup for SystemDns {
    fn lookup<'a>(&'a self, host: &'a str) -> BoxFuture<'a, NetworkDns> {
        Box::pin(async move {
            let resolver = match &self.resolver {
                Ok(r) => r,
                Err(e) => {
                    return NetworkDns {
                        error: Some(clean(e)),
                        ..NetworkDns::default()
                    }
                }
            };
            let mut records: Vec<Record> = Vec::new();
            let mut errors: Vec<String> = Vec::new();
            for (rt, label) in [(RecordType::A, "A"), (RecordType::AAAA, "AAAA")] {
                match tokio::time::timeout(LOOKUP_TIMEOUT, resolver.lookup(host, rt)).await {
                    Ok(Ok(found)) => records.extend(found.answers().iter().cloned()),
                    // "No records of this type" is an answer, not a failure.
                    Ok(Err(e)) if e.is_no_records_found() => {}
                    Ok(Err(e)) => errors.push(format!("{label}: {}", clean(&e.to_string()))),
                    Err(_) => errors.push(format!("{label}: timed out after 3 s")),
                }
            }
            let mut dns = fold_records(&records, Some(self.label.clone()));
            if !errors.is_empty() {
                dns.error = Some(errors.join("; "));
            }
            dns
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_resolver::proto::rr::rdata::{A, AAAA, CNAME};
    use hickory_resolver::proto::rr::Name;
    use std::str::FromStr;

    fn rec(name: &str, ttl: u32, data: RData) -> Record {
        Record::from_rdata(Name::from_str(name).unwrap(), ttl, data)
    }

    #[test]
    fn folds_cname_chain_addresses_and_lowest_ttl() {
        let records = vec![
            rec(
                "tv.example.",
                300,
                RData::CNAME(CNAME(Name::from_str("tv.Example.AKAMAIEDGE.net.").unwrap())),
            ),
            rec(
                "tv.example.akamaiedge.net.",
                20,
                RData::A(A("192.0.2.7".parse().unwrap())),
            ),
            rec(
                "tv.example.akamaiedge.net.",
                60,
                RData::AAAA(AAAA("2001:db8::7".parse().unwrap())),
            ),
            rec(
                "tv.example.akamaiedge.net.",
                20,
                RData::A(A("192.0.2.7".parse().unwrap())),
            ),
        ];
        let d = fold_records(&records, Some("192.0.2.53".into()));
        assert_eq!(d.cname, vec!["tv.example.akamaiedge.net"]);
        assert_eq!(d.a, vec!["192.0.2.7"]);
        assert_eq!(d.aaaa, vec!["2001:db8::7"]);
        assert_eq!(d.ttl, Some(20));
        assert_eq!(d.resolver.as_deref(), Some("192.0.2.53"));
        assert_eq!(d.error, None);
    }

    #[test]
    fn empty_answer_invents_nothing() {
        let d = fold_records(&[], None);
        assert_eq!(d, NetworkDns::default());
        assert_eq!(d.ttl, None);
    }
}
