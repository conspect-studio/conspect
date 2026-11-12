//! Cloudflare DNS-01 provider (ADR-0107 §5a): API token or global key.
//!
//! Every API call goes through the screened [`EgressClient`] with the public
//! policy, a bounded body and fixed deadlines. Credentials are loaded from an
//! environment variable NAMED in config, or from `<state_dir>/secrets/<NAME>`;
//! they are zeroized on drop and never appear in `Debug`, logs or errors.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use multiview_config::DnsProviderConfig;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use super::dns::{DnsFuture, DnsProvider, TxtHandle};
use super::egress::{EgressClient, EgressPolicy, EgressTimeouts, DNS_API_BODY_CAP};
use super::AcmeError;

/// The production API origin.
pub const API_ORIGIN: &str = "https://api.cloudflare.com/client/v4";
/// TTL of a challenge record.
const TXT_TTL: u32 = 60;
/// Per-query deadline at an authoritative server.
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
/// Propagation poll interval.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A Cloudflare credential. Zeroized on drop; `Debug` is redacted.
pub enum CloudflareCredential {
    /// Scoped API token (`Authorization: Bearer`), recommended.
    Token(Zeroizing<String>),
    /// Account email + global API key (`X-Auth-Email` + `X-Auth-Key`).
    GlobalKey {
        /// Account email.
        email: Zeroizing<String>,
        /// Global API key.
        key: Zeroizing<String>,
    },
}

impl std::fmt::Debug for CloudflareCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token(_) => f.write_str("CloudflareCredential::Token([redacted])"),
            Self::GlobalKey { .. } => f.write_str("CloudflareCredential::GlobalKey([redacted])"),
        }
    }
}

/// Where secrets named in config are looked up: the environment variable of
/// that NAME first, else `<state_dir>/secrets/<NAME>`.
#[derive(Debug, Clone)]
pub struct SecretLookup {
    state_dir: std::path::PathBuf,
}

impl SecretLookup {
    /// Look secrets up for `state_dir`.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// Resolve one secret by NAME. Empty values count as absent.
    ///
    /// # Errors
    /// [`AcmeError::Storage`] when the secrets file exists but is insecure.
    pub fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>, AcmeError> {
        if let Ok(value) = std::env::var(name) {
            let value = Zeroizing::new(value);
            if !value.is_empty() {
                return Ok(Some(value));
            }
        }
        Ok(super::store::read_secret_file(&self.state_dir, name)?.filter(|v| !v.is_empty()))
    }
}

impl CloudflareCredential {
    /// Load the credential configured in `[network.tls.dns]`.
    ///
    /// # Errors
    /// [`AcmeError::Credential`] when a named secret is absent; [`AcmeError::Storage`]
    /// when its secrets file is insecure.
    pub fn from_config(
        config: &DnsProviderConfig,
        secrets: &SecretLookup,
    ) -> Result<Self, AcmeError> {
        match config {
            DnsProviderConfig::Cloudflare {
                token_env: Some(token),
                ..
            } => Ok(Self::Token(
                secrets.get(token)?.ok_or(AcmeError::Credential)?,
            )),
            DnsProviderConfig::Cloudflare {
                email_env: Some(email),
                api_key_env: Some(key),
                ..
            } => Ok(Self::GlobalKey {
                email: secrets.get(email)?.ok_or(AcmeError::Credential)?,
                key: secrets.get(key)?.ok_or(AcmeError::Credential)?,
            }),
            DnsProviderConfig::Cloudflare { .. } => Err(AcmeError::Credential),
        }
    }
}

/// Where authoritative nameservers come from.
#[derive(Debug, Clone)]
enum Authoritative {
    /// NS query for the zone on the system resolver.
    System,
    /// Fixed server list (test seam).
    #[cfg_attr(not(feature = "_test-seams"), allow(dead_code))]
    Fixed(Vec<SocketAddr>),
}

/// A resolved zone.
#[derive(Debug, Clone)]
struct Zone {
    id: String,
    name: String,
}

/// The Cloudflare provider.
pub struct CloudflareDns {
    client: EgressClient,
    origin: String,
    credential: CloudflareCredential,
    authoritative: Authoritative,
    ns_policy: EgressPolicy,
    /// Last zone looked up per suffix in the current publish call.
    zone_cache: Mutex<Vec<(String, Zone)>>,
}

impl std::fmt::Debug for CloudflareDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudflareDns")
            .field("origin", &self.origin)
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

fn path_safe(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl CloudflareDns {
    /// The production provider against [`API_ORIGIN`].
    ///
    /// # Errors
    /// [`AcmeError::Egress`] if the HTTP client cannot be built.
    pub fn new(credential: CloudflareCredential) -> Result<Self, AcmeError> {
        Ok(Self {
            client: EgressClient::new(
                EgressPolicy::public(),
                None,
                EgressTimeouts::default(),
                DNS_API_BODY_CAP,
            )?,
            origin: API_ORIGIN.to_owned(),
            credential,
            authoritative: Authoritative::System,
            ns_policy: EgressPolicy::public(),
            zone_cache: Mutex::new(Vec::new()),
        })
    }

    /// TEST SEAM: a provider against a local fake API (`origin`, e.g.
    /// `https://[::1]:<port>/client/v4`, trusted through `trust_anchor`) and fixed
    /// authoritative servers. Loopback is admitted on `api_port` and the
    /// authoritative ports only.
    ///
    /// # Errors
    /// [`AcmeError::Egress`] if the HTTP client cannot be built.
    #[cfg(feature = "_test-seams")]
    pub fn for_tests(
        credential: CloudflareCredential,
        origin: &str,
        api_port: u16,
        trust_anchor: Option<&Path>,
        authoritative: Vec<SocketAddr>,
    ) -> Result<Self, AcmeError> {
        let ns_ports: Vec<u16> = authoritative.iter().map(SocketAddr::port).collect();
        Ok(Self {
            client: EgressClient::new(
                EgressPolicy::for_tests_allowing_loopback_ports(false, &[api_port]),
                trust_anchor,
                EgressTimeouts::default(),
                DNS_API_BODY_CAP,
            )?,
            origin: origin.trim_end_matches('/').to_owned(),
            credential,
            authoritative: Authoritative::Fixed(authoritative),
            ns_policy: EgressPolicy::for_tests_allowing_loopback_ports(false, &ns_ports),
            zone_cache: Mutex::new(Vec::new()),
        })
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<reqwest::Request, AcmeError> {
        let mut builder = self
            .client
            .reqwest()
            .request(method, format!("{}{path}", self.origin))
            .query(query)
            .header(reqwest::header::ACCEPT, "application/json");
        builder = match &self.credential {
            CloudflareCredential::Token(token) => builder.bearer_auth(token.as_str()),
            CloudflareCredential::GlobalKey { email, key } => builder
                .header("X-Auth-Email", email.as_str())
                .header("X-Auth-Key", key.as_str()),
        };
        if let Some(body) = body {
            builder = builder.json(body);
        }
        builder.build().map_err(|_| AcmeError::DnsApi)
    }

    /// Send one API call; `Ok(None)` for a 404 when `allow_404`.
    async fn call(
        &self,
        request: reqwest::Request,
        allow_404: bool,
    ) -> Result<Option<Value>, AcmeError> {
        let response = self
            .client
            .send(request)
            .await
            .map_err(|e| e.category(AcmeError::DnsApi))?;
        let status = response.status;
        if allow_404 && status == http::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == http::StatusCode::UNAUTHORIZED || status == http::StatusCode::FORBIDDEN {
            return Err(AcmeError::Credential);
        }
        if status == http::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            return Err(AcmeError::DnsApi);
        }
        let json: Value = serde_json::from_slice(&response.body).map_err(|_| AcmeError::DnsApi)?;
        if !status.is_success() || json.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(AcmeError::DnsApi);
        }
        Ok(Some(json))
    }

    async fn call_ok(&self, request: reqwest::Request) -> Result<Value, AcmeError> {
        self.call(request, false).await?.ok_or(AcmeError::DnsApi)
    }

    /// Find the zone by LONGEST suffix of `name` (wildcard stripped).
    async fn zone(&self, name: &str) -> Result<Zone, AcmeError> {
        let bare = name
            .strip_prefix("*.")
            .unwrap_or(name)
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if let Some((_, zone)) = self
            .zone_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(n, _)| *n == bare)
        {
            return Ok(zone.clone());
        }
        let labels: Vec<&str> = bare.split('.').collect();
        // Longest suffix first; never query a bare TLD.
        for start in 0..labels.len().saturating_sub(1) {
            let suffix = labels.get(start..).unwrap_or_default().join(".");
            let request = self.request(
                reqwest::Method::GET,
                "/zones",
                &[("name", suffix.as_str()), ("status", "active")],
                None,
            )?;
            let json = self.call_ok(request).await?;
            let Some(found) = json
                .get("result")
                .and_then(Value::as_array)
                .and_then(|zones| {
                    zones.iter().find(|z| {
                        z.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|n| n.eq_ignore_ascii_case(&suffix))
                    })
                })
            else {
                continue;
            };
            let id = found
                .get("id")
                .and_then(Value::as_str)
                .ok_or(AcmeError::DnsApi)?;
            if !path_safe(id) {
                return Err(AcmeError::DnsApi);
            }
            let zone = Zone {
                id: id.to_owned(),
                name: suffix,
            };
            let mut cache = self
                .zone_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cache.len() >= 64 {
                cache.clear();
            }
            cache.push((bare, zone.clone()));
            return Ok(zone);
        }
        // The credential cannot see a zone for this name.
        Err(AcmeError::Credential)
    }

    async fn nameserver_addrs(&self, zone: &str) -> Result<Vec<SocketAddr>, AcmeError> {
        let candidates = match &self.authoritative {
            Authoritative::Fixed(list) => list.clone(),
            Authoritative::System => system_nameservers(zone).await?,
        };
        let screened: Vec<SocketAddr> = candidates
            .into_iter()
            .filter(|a| self.ns_policy.screen_ip(a.ip(), a.port()).is_ok())
            .collect();
        if screened.is_empty() {
            return Err(AcmeError::Propagation);
        }
        Ok(screened)
    }
}

async fn system_nameservers(zone: &str) -> Result<Vec<SocketAddr>, AcmeError> {
    use hickory_resolver::proto::rr::{RData, RecordType};
    let (config, mut opts) =
        hickory_resolver::system_conf::read_system_conf().map_err(|_| AcmeError::Propagation)?;
    opts.timeout = QUERY_TIMEOUT;
    opts.attempts = 2;
    let resolver = hickory_resolver::TokioResolver::builder_with_config(
        config,
        hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
    )
    .with_options(opts)
    .build()
    .map_err(|_| AcmeError::Propagation)?;
    let ns = tokio::time::timeout(QUERY_TIMEOUT * 3, resolver.lookup(zone, RecordType::NS))
        .await
        .map_err(|_| AcmeError::Propagation)?
        .map_err(|_| AcmeError::Propagation)?;
    let names: Vec<String> = ns
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            RData::NS(ns) => Some(ns.0.to_ascii()),
            _ => None,
        })
        .take(16)
        .collect();
    let mut addrs = Vec::new();
    for name in names {
        if let Ok(Ok(ips)) =
            tokio::time::timeout(QUERY_TIMEOUT * 2, resolver.lookup_ip(name.as_str())).await
        {
            addrs.extend(ips.iter().map(|ip: IpAddr| SocketAddr::new(ip, 53)));
        }
    }
    Ok(addrs)
}

/// Ask one authoritative server directly (UDP, then TCP; no recursion).
async fn served_at(server: SocketAddr, fqdn: &str, value: &str) -> bool {
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig};
    use hickory_resolver::proto::rr::{RData, RecordType};
    let mut udp = ConnectionConfig::udp();
    udp.port = server.port();
    let mut tcp = ConnectionConfig::tcp();
    tcp.port = server.port();
    let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
        server.ip(),
        true,
        vec![udp, tcp],
    )]);
    let mut builder = hickory_resolver::TokioResolver::builder_with_config(
        config,
        hickory_resolver::net::runtime::TokioRuntimeProvider::default(),
    );
    let opts = builder.options_mut();
    opts.timeout = QUERY_TIMEOUT;
    opts.attempts = 1;
    opts.recursion_desired = false;
    opts.cache_size = 0;
    opts.use_hosts_file = hickory_resolver::config::ResolveHosts::Never;
    opts.ndots = 0;
    let Ok(resolver) = builder.build() else {
        return false;
    };
    let name = format!("{}.", fqdn.trim_end_matches('.'));
    let Ok(Ok(found)) = tokio::time::timeout(
        QUERY_TIMEOUT * 2,
        resolver.lookup(name.as_str(), RecordType::TXT),
    )
    .await
    else {
        return false;
    };
    found.answers().iter().any(|record| match &record.data {
        RData::TXT(txt) => txt
            .txt_data
            .iter()
            .flat_map(|part| part.iter())
            .copied()
            .eq(value.bytes()),
        _ => false,
    })
}

impl DnsProvider for CloudflareDns {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn validate_credential<'a>(&'a self, names: &'a [String]) -> DnsFuture<'a, ()> {
        Box::pin(async move {
            if matches!(self.credential, CloudflareCredential::Token(_)) {
                let request =
                    self.request(reqwest::Method::GET, "/user/tokens/verify", &[], None)?;
                let json = self.call_ok(request).await?;
                let active = json
                    .get("result")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    == Some("active");
                if !active {
                    return Err(AcmeError::Credential);
                }
            }
            for name in names {
                if name.parse::<IpAddr>().is_ok() {
                    continue;
                }
                self.zone(name).await?;
            }
            Ok(())
        })
    }

    fn publish_txt<'a>(&'a self, fqdn: &'a str, value: &'a str) -> DnsFuture<'a, TxtHandle> {
        Box::pin(async move {
            let bare = fqdn.strip_prefix("_acme-challenge.").unwrap_or(fqdn);
            let zone = self.zone(bare).await?;
            let path = format!("/zones/{}/dns_records", zone.id);
            let body = json!({"type": "TXT", "name": fqdn, "content": value, "ttl": TXT_TTL});
            let request = self.request(reqwest::Method::POST, &path, &[], Some(&body))?;
            let json = self.call_ok(request).await?;
            let id = json
                .get("result")
                .and_then(|r| r.get("id"))
                .and_then(Value::as_str)
                .ok_or(AcmeError::DnsApi)?;
            if !path_safe(id) {
                return Err(AcmeError::DnsApi);
            }
            Ok(TxtHandle(format!("{}/{id}", zone.id)))
        })
    }

    fn confirm_at_authoritative<'a>(
        &'a self,
        fqdn: &'a str,
        value: &'a str,
        deadline: Duration,
    ) -> DnsFuture<'a, ()> {
        Box::pin(async move {
            let until = tokio::time::Instant::now() + deadline;
            let bare = fqdn.strip_prefix("_acme-challenge.").unwrap_or(fqdn);
            let zone = self.zone(bare).await?;
            let servers = tokio::time::timeout_at(until, self.nameserver_addrs(&zone.name))
                .await
                .map_err(|_| AcmeError::Propagation)??;
            loop {
                let mut all = true;
                for server in &servers {
                    if !served_at(*server, fqdn, value).await {
                        all = false;
                        break;
                    }
                }
                if all {
                    return Ok(());
                }
                if tokio::time::Instant::now() + POLL_INTERVAL >= until {
                    return Err(AcmeError::Propagation);
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
    }

    fn remove_txt<'a>(&'a self, handle: &'a TxtHandle) -> DnsFuture<'a, ()> {
        Box::pin(async move {
            let (zone, id) = handle.0.split_once('/').ok_or(AcmeError::DnsApi)?;
            if !path_safe(zone) || !path_safe(id) {
                return Err(AcmeError::DnsApi);
            }
            let path = format!("/zones/{zone}/dns_records/{id}");
            let request = self.request(reqwest::Method::DELETE, &path, &[], None)?;
            self.call(request, true).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_credentials() {
        let token = CloudflareCredential::Token(Zeroizing::new("tok-SECRET".into()));
        let key = CloudflareCredential::GlobalKey {
            email: Zeroizing::new("ops@SECRET.example".into()),
            key: Zeroizing::new("key-SECRET".into()),
        };
        for c in [&token, &key] {
            let d = format!("{c:?}");
            assert!(!d.contains("SECRET"), "{d}");
            assert!(d.contains("redacted"));
        }
        let lookup = SecretLookup::new(Path::new("/nonexistent"));
        assert!(!format!("{lookup:?}").contains("SECRET"));
    }

    #[test]
    fn handle_ids_are_path_safe() {
        assert!(path_safe("023e105f4ecef8ad9ca31a8372d0c353"));
        for bad in ["", "../x", "a/b", "a?b", "a b", &"a".repeat(65)] {
            assert!(!path_safe(bad), "{bad}");
        }
    }

    #[test]
    fn from_config_reads_named_env_or_absent_is_credential_error() {
        let lookup = SecretLookup::new(Path::new("/nonexistent-acme-state"));
        let cfg = DnsProviderConfig::Cloudflare {
            token_env: Some("CONSPECT_TEST_ABSENT_CF_TOKEN_9F3".into()),
            email_env: None,
            api_key_env: None,
            propagation_timeout_s: 120,
        };
        assert_eq!(
            CloudflareCredential::from_config(&cfg, &lookup).unwrap_err(),
            AcmeError::Credential
        );
    }
}
