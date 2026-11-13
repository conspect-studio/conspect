//! Cloudflare DNS-01 provider (ADR-0107 §5a): API token or global key.
//!
//! Every API call goes through the screened [`EgressClient`] with the public
//! policy, a bounded body and fixed deadlines. Credentials are loaded from an
//! environment variable NAMED in config, or from `<state_dir>/secrets/<NAME>`;
//! they are zeroized on drop and never appear in `Debug`, logs or errors.
//!
//! Before the CA is told to validate, the TXT record is confirmed either at
//! every authoritative nameserver directly (port 53) or, when port 53 is
//! intercepted by the network, through DNS over HTTPS (RFC 8484) at
//! Cloudflare's own resolver ([`DnsConfirm`]).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use multiview_config::{DnsConfirm, DnsProviderConfig};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use super::dns::{DnsFuture, DnsProvider, TxtHandle};
use super::egress::{EgressClient, EgressPolicy, EgressTimeouts, DNS_API_BODY_CAP};
use super::AcmeError;

/// The production API origin.
pub const API_ORIGIN: &str = "https://api.cloudflare.com/client/v4";
/// The production DNS-over-HTTPS endpoint (RFC 8484), Cloudflare's own resolver.
pub const DOH_URL: &str = "https://cloudflare-dns.com/dns-query";
/// Interception probe targets: RFC 5737 / RFC 3849 documentation addresses
/// where nothing listens. Any answer from them means a middlebox answers
/// port 53 on their behalf.
const PROBE_ADDRS: [SocketAddr; 2] = [
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 53),
    SocketAddr::new(
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
        53,
    ),
];
/// How long the interception probe waits for any reply.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long one interception verdict is reused by this provider instance.
const DETECTION_TTL: Duration = Duration::from_secs(10 * 60);
/// Body cap of one `DoH` answer.
const DOH_BODY_CAP: usize = 64 * 1024;
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

/// How one confirmation is carried out (the resolved [`DnsConfirm`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    /// Every authoritative nameserver, directly on port 53.
    Authoritative,
    /// DNS over HTTPS at Cloudflare's resolver.
    Doh,
}

/// TEST SEAM: where the confirmation goes in a test (feature `_test-seams`).
#[cfg(feature = "_test-seams")]
#[derive(Debug, Clone)]
pub struct TestConfirm {
    /// The configured mode.
    pub mode: DnsConfirm,
    /// Interception probe targets (replacing the documentation addresses).
    pub probe: Vec<SocketAddr>,
    /// `DoH` endpoint, e.g. `https://[::1]:<port>/dns-query`, trusted through
    /// the provider's `trust_anchor`.
    pub doh_url: String,
    /// Loopback port of the `DoH` endpoint (admitted by the egress screen).
    pub doh_port: u16,
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
    /// Configured confirmation mode.
    confirm: DnsConfirm,
    /// Interception probe targets.
    probe: Vec<SocketAddr>,
    /// `DoH` client (public policy, bounded body).
    doh_client: EgressClient,
    /// `DoH` endpoint.
    doh_url: String,
    /// Cached `auto` verdict: (when, method).
    detected: Mutex<Option<(std::time::Instant, Method)>>,
    /// The method of the last confirmation (for the failure detail).
    last_method: Mutex<Option<Method>>,
    /// Why the last failed `DoH` confirmation's final poll did not see the
    /// value (for the failure detail).
    last_doh: Mutex<Option<DohOutcome>>,
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
            confirm: DnsConfirm::Auto,
            probe: PROBE_ADDRS.to_vec(),
            doh_client: EgressClient::new_unpooled(
                EgressPolicy::public(),
                None,
                EgressTimeouts::default(),
                DOH_BODY_CAP,
            )?,
            doh_url: DOH_URL.to_owned(),
            detected: Mutex::new(None),
            last_method: Mutex::new(None),
            last_doh: Mutex::new(None),
        })
    }

    /// Set the confirmation mode (`[network.tls.dns] confirm`, default `auto`).
    #[must_use]
    pub fn with_confirm(mut self, confirm: DnsConfirm) -> Self {
        self.confirm = confirm;
        self
    }

    /// TEST SEAM: a provider against a local fake API (`origin`, e.g.
    /// `https://[::1]:<port>/client/v4`, trusted through `trust_anchor`) and fixed
    /// authoritative servers. Loopback is admitted on `api_port` and the
    /// authoritative ports only.
    ///
    /// # Errors
    /// [`AcmeError::Egress`] if the HTTP client cannot be built.
    ///
    /// Confirmation is `authoritative` (the fixed servers), as before the
    /// `confirm` knob existed; [`Self::for_tests_with_confirm`] selects another.
    #[cfg(feature = "_test-seams")]
    pub fn for_tests(
        credential: CloudflareCredential,
        origin: &str,
        api_port: u16,
        trust_anchor: Option<&Path>,
        authoritative: Vec<SocketAddr>,
    ) -> Result<Self, AcmeError> {
        Self::for_tests_with_confirm(
            credential,
            origin,
            api_port,
            trust_anchor,
            authoritative,
            TestConfirm {
                mode: DnsConfirm::Authoritative,
                probe: Vec::new(),
                doh_url: "https://[::1]:9/dns-query".to_owned(),
                doh_port: 9,
            },
        )
    }

    /// TEST SEAM: as [`Self::for_tests`], with the confirmation mode, the
    /// interception probe targets and a loopback `DoH` endpoint injected.
    ///
    /// # Errors
    /// [`AcmeError::Egress`] if an HTTP client cannot be built.
    #[cfg(feature = "_test-seams")]
    pub fn for_tests_with_confirm(
        credential: CloudflareCredential,
        origin: &str,
        api_port: u16,
        trust_anchor: Option<&Path>,
        authoritative: Vec<SocketAddr>,
        confirm: TestConfirm,
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
            confirm: confirm.mode,
            probe: confirm.probe,
            doh_client: EgressClient::new_unpooled(
                EgressPolicy::for_tests_allowing_loopback_ports(false, &[confirm.doh_port]),
                trust_anchor,
                EgressTimeouts::default(),
                DOH_BODY_CAP,
            )?,
            doh_url: confirm.doh_url,
            detected: Mutex::new(None),
            last_method: Mutex::new(None),
            last_doh: Mutex::new(None),
        })
    }

    /// Resolve the confirmation method: the configured one, or for `auto`
    /// the (cached, bounded) interception verdict.
    async fn method(&self, zone: &str) -> Method {
        match self.confirm {
            DnsConfirm::Authoritative => return Method::Authoritative,
            DnsConfirm::Doh => return Method::Doh,
            DnsConfirm::Auto => {}
        }
        if let Some((at, method)) = *self
            .detected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            if at.elapsed() < DETECTION_TTL {
                return method;
            }
        }
        let intercepted = port53_intercepted(&self.probe, zone).await;
        let method = if intercepted {
            tracing::info!(
                method = "doh",
                "acme: DNS-01 confirmation via DNS over HTTPS (cloudflare-dns.com): a port-53 \
                 query to a documentation address where nothing listens was answered, so this \
                 network intercepts DNS and the authoritative nameservers cannot be asked directly"
            );
            Method::Doh
        } else {
            tracing::info!(
                method = "authoritative",
                "acme: DNS-01 confirmation at the authoritative nameservers (port 53 is not \
                 intercepted: the interception probe got no answer)"
            );
            Method::Authoritative
        };
        *self
            .detected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((std::time::Instant::now(), method));
        method
    }

    /// One `DoH` lookup: whether the resolver answers `value` at `fqdn` now,
    /// and if not, why.
    async fn seen_via_doh(&self, fqdn: &str, value: &str) -> DohOutcome {
        doh_lookup(&self.doh_client, &self.doh_url, fqdn, value).await
    }

    /// Poll `DoH` until `value` is seen or `until` passes.
    ///
    /// 1.1.1.1 (`cloudflare-dns.com`) is Cloudflare's own resolver, adjacent
    /// to the authoritative servers of a Cloudflare zone, and the challenge
    /// record's TTL is 60 s, so a stale positive answer ages out quickly.
    /// The trade-off: this is a resolver's view, not a check of each
    /// authoritative server; it is used only where port 53 cannot reach
    /// those servers at all. The first query waits one poll interval after
    /// publish so it does not race the edge and cache a negative answer.
    async fn confirm_via_doh(
        &self,
        fqdn: &str,
        value: &str,
        until: tokio::time::Instant,
    ) -> Result<(), AcmeError> {
        let mut log = DohLog::default();
        loop {
            if tokio::time::Instant::now() + POLL_INTERVAL >= until {
                log.give_up(fqdn);
                self.set_last_doh(log.last.as_ref());
                return Err(AcmeError::Propagation);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let outcome = tokio::time::timeout_at(until, self.seen_via_doh(fqdn, value))
                .await
                .unwrap_or(DohOutcome::Deadline);
            if outcome == DohOutcome::Seen {
                log.seen(fqdn);
                return Ok(());
            }
            log.not_seen(fqdn, outcome);
        }
    }

    fn set_last_doh(&self, outcome: Option<&DohOutcome>) {
        *self
            .last_doh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = outcome.cloned();
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

/// DNS record type TXT.
const TXT_TYPE: u16 = 16;
/// DNS record type SOA.
const SOA_TYPE: u16 = 6;

/// One query message in wire format (`None` for an invalid name).
fn query_wire(name: &str, record_type: u16, id: u16, recursion: bool) -> Option<Vec<u8>> {
    use hickory_resolver::proto::op::{Message, MessageType, OpCode, Query};
    use hickory_resolver::proto::rr::{Name, RecordType};
    let name = Name::from_ascii(format!("{}.", name.trim_end_matches('.'))).ok()?;
    let mut message = Message::new(id, MessageType::Query, OpCode::Query);
    message.metadata.recursion_desired = recursion;
    message.add_query(Query::query(name, RecordType::from(record_type)));
    message.to_vec().ok()
}

/// Whether a DNS answer (wire format) carries a TXT record whose
/// concatenated character-strings equal `value` byte for byte (owner
/// unchecked; the classifier's own tests).
#[cfg(test)]
fn txt_answer_matches(wire: &[u8], value: &str) -> bool {
    use hickory_resolver::proto::op::Message;
    Message::from_vec(wire).is_ok_and(|m| classify_doh_answer(&m, None, value) == DohOutcome::Seen)
}

/// The result of one `DoH` poll. Every variant but [`Self::Seen`] is a
/// bounded, secret-free reason why the value was not seen (no names, no
/// values, no URLs; counts are small integers from the answer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DohOutcome {
    /// The resolver answered `value`.
    Seen,
    /// The query could not be built (invalid name).
    BadName,
    /// The HTTP request failed (egress screen, TLS, connect, timeout, body cap).
    Egress(super::egress::EgressFailure),
    /// The resolver answered with a non-2xx HTTP status.
    Status(u16),
    /// The body is not a DNS message.
    Unparsable,
    /// The answer is not a response to this query (id, QR, or question).
    Mismatched,
    /// A DNS answer without the value: response code, answer count, TXT count.
    NotSeen {
        /// DNS response code (0 = `NOERROR`, 3 = `NXDOMAIN`, 2 = `SERVFAIL`).
        rcode: u16,
        /// Records in the answer section.
        answers: usize,
        /// TXT records in the answer section (none equal to the value).
        txt: usize,
    },
    /// The poll was cut off by the confirmation deadline.
    Deadline,
}

impl std::fmt::Display for DohOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seen => f.write_str("value present"),
            Self::BadName => f.write_str("invalid query name"),
            Self::Egress(failure) => write!(f, "request failed: {failure}"),
            Self::Status(code) => write!(f, "HTTP status {code}"),
            Self::Unparsable => f.write_str("body is not a DNS message"),
            Self::Mismatched => f.write_str("answer does not match the query"),
            Self::NotSeen {
                rcode,
                answers,
                txt,
            } => write!(
                f,
                "rcode {rcode} ({}), {answers} answer(s), {txt} TXT, value absent",
                rcode_name(*rcode)
            ),
            Self::Deadline => f.write_str("cut off by the deadline"),
        }
    }
}

impl DohOutcome {
    /// A short stable class for log de-duplication (same class = same line).
    fn class(&self) -> (u8, u16, &'static str) {
        match self {
            Self::Seen => (0, 0, ""),
            Self::BadName => (1, 0, ""),
            Self::Egress(f) => (2, 0, f.reason),
            Self::Status(code) => (3, *code, ""),
            Self::Unparsable => (4, 0, ""),
            Self::Mismatched => (5, 0, ""),
            Self::NotSeen { rcode, .. } => (6, *rcode, ""),
            Self::Deadline => (7, 0, ""),
        }
    }
}

const fn rcode_name(rcode: u16) -> &'static str {
    match rcode {
        0 => "NOERROR",
        1 => "FORMERR",
        2 => "SERVFAIL",
        3 => "NXDOMAIN",
        4 => "NOTIMP",
        5 => "REFUSED",
        _ => "other",
    }
}

/// Most "not seen" log lines in one confirmation (a change of reason logs
/// again, up to this bound; the final reason is always logged on give-up).
const DOH_LOG_CAP: usize = 8;

/// Bounded logging across the polls of one confirmation: the first poll's
/// reason, every change of reason (at most [`DOH_LOG_CAP`] lines), and the
/// final verdict.
#[derive(Debug, Default)]
struct DohLog {
    polls: u32,
    logged: usize,
    last: Option<DohOutcome>,
}

impl DohLog {
    fn not_seen(&mut self, fqdn: &str, outcome: DohOutcome) {
        self.polls = self.polls.saturating_add(1);
        let changed = self.last.as_ref().map(DohOutcome::class) != Some(outcome.class());
        if changed && self.logged < DOH_LOG_CAP {
            self.logged += 1;
            tracing::info!(
                fqdn,
                poll = self.polls,
                reason = %outcome,
                "acme: DNS-01 DoH poll did not see the TXT value yet"
            );
        }
        self.last = Some(outcome);
    }

    fn seen(&self, fqdn: &str) {
        tracing::info!(
            fqdn,
            polls = self.polls.saturating_add(1),
            "acme: DNS-01 TXT value seen via DNS over HTTPS"
        );
    }

    fn give_up(&self, fqdn: &str) {
        let reason = self
            .last
            .as_ref()
            .map_or_else(|| "no poll completed".to_owned(), ToString::to_string);
        tracing::warn!(
            fqdn,
            polls = self.polls,
            last = %reason,
            "acme: DNS-01 TXT value not seen via DNS over HTTPS before the deadline"
        );
    }
}

/// One RFC 8484 GET for `fqdn` TXT through `client` at `url`, classified.
/// This is exactly the engine's confirmation path (also used by
/// `multiview tls doh-check`).
pub async fn doh_lookup(client: &EgressClient, url: &str, fqdn: &str, value: &str) -> DohOutcome {
    use base64::Engine as _;
    // RFC 8484 §4.1: id 0 keeps the GET cache-friendly.
    let Some(wire) = query_wire(fqdn, TXT_TYPE, 0, true) else {
        return DohOutcome::BadName;
    };
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(wire);
    let Ok(request) = client
        .reqwest()
        .get(url)
        .query(&[("dns", encoded.as_str())])
        .header(reqwest::header::ACCEPT, "application/dns-message")
        .build()
    else {
        return DohOutcome::BadName;
    };
    let response = match client.send_explained(request).await {
        Ok(response) => response,
        Err(failure) => return DohOutcome::Egress(failure),
    };
    if !response.status.is_success() {
        return DohOutcome::Status(response.status.as_u16());
    }
    classify_doh_answer_for(&response.body, fqdn, value)
}

/// The production `DoH` client: public egress policy, built-in roots, the
/// same deadlines and body cap as the engine's confirmation.
///
/// # Errors
/// [`AcmeError::Egress`] if the client cannot be built.
pub fn production_doh_client() -> Result<EgressClient, AcmeError> {
    EgressClient::new_unpooled(
        EgressPolicy::public(),
        None,
        EgressTimeouts::default(),
        DOH_BODY_CAP,
    )
}

/// The `DoH` endpoint host's addresses as the engine's resolver sees them,
/// each with the egress screen's verdict (`None` = admitted, else the class).
/// Diagnostic for `multiview tls doh-check`.
///
/// # Errors
/// The resolver's error text when the name does not resolve.
pub async fn doh_host_screen() -> Result<Vec<(IpAddr, Option<&'static str>)>, String> {
    let host = url::Url::parse(DOH_URL)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let policy = EgressPolicy::public();
    let addrs = tokio::net::lookup_host((host.as_str(), 443))
        .await
        .map_err(|e| e.to_string())?;
    Ok(addrs
        .map(|a| (a.ip(), policy.screen_ip(a.ip(), 0).err().map(|r| r.0)))
        .collect())
}

/// Classify a `DoH` answer to the TXT query for `fqdn`.
fn classify_doh_answer_for(wire: &[u8], fqdn: &str, value: &str) -> DohOutcome {
    use hickory_resolver::proto::op::{Message, MessageType};
    use hickory_resolver::proto::rr::{Name, RecordType};
    let Ok(message) = Message::from_vec(wire) else {
        return DohOutcome::Unparsable;
    };
    let expected = Name::from_ascii(format!("{}.", fqdn.trim_end_matches('.'))).ok();
    let question_ok = message.queries.len() == 1
        && message.queries.first().is_some_and(|q| {
            q.query_type() == RecordType::TXT
                && expected
                    .as_ref()
                    .is_some_and(|n| n.to_lowercase() == q.name().to_lowercase())
        });
    if message.metadata.message_type != MessageType::Response
        || message.metadata.id != 0
        || !question_ok
    {
        return DohOutcome::Mismatched;
    }
    classify_doh_answer(&message, expected.as_ref(), value)
}

/// Classify the answer section of a DNS message against `value`. With
/// `owner`, only an IN TXT record owned by exactly that name (case-insensitive)
/// counts; a TXT at any other owner is counted but never "seen".
fn classify_doh_answer(
    message: &hickory_resolver::proto::op::Message,
    owner: Option<&hickory_resolver::proto::rr::Name>,
    value: &str,
) -> DohOutcome {
    use hickory_resolver::proto::rr::{DNSClass, RData};
    let mut txt = 0;
    for record in &message.answers {
        if let RData::TXT(data) = &record.data {
            txt += 1;
            let at_owner = owner.is_none_or(|expected| {
                record.dns_class == DNSClass::IN
                    && expected.to_lowercase() == record.name.to_lowercase()
            });
            if at_owner
                && data
                    .txt_data
                    .iter()
                    .flat_map(|part| part.iter())
                    .copied()
                    .eq(value.bytes())
            {
                return DohOutcome::Seen;
            }
        }
    }
    DohOutcome::NotSeen {
        rcode: u16::from(message.metadata.response_code),
        answers: message.answers.len(),
        txt,
    }
}

/// Whether `wire`, received from `from`, is an answer to the probe query
/// `id`: a DNS message with QR=response and the same id, sent from one of the
/// `probe` targets (address compared in canonical form, so a v4-mapped sender
/// matches its IPv4 target).
fn is_probe_reply(wire: &[u8], from: SocketAddr, id: u16, probe: &[SocketAddr]) -> bool {
    use hickory_resolver::proto::op::{Message, MessageType};
    let from = SocketAddr::new(from.ip().to_canonical(), from.port());
    if !probe
        .iter()
        .any(|t| SocketAddr::new(t.ip().to_canonical(), t.port()) == from)
    {
        return false;
    }
    Message::from_vec(wire)
        .is_ok_and(|m| m.metadata.message_type == MessageType::Response && m.metadata.id == id)
}

/// Whether port 53 is intercepted: one SOA query for `zone` (random id) to
/// every probe target, where nothing listens; a reply to that query (QR set,
/// same id, from a probe target) within [`PROBE_TIMEOUT`] means a middlebox
/// answered on their behalf.
async fn port53_intercepted(probe: &[SocketAddr], zone: &str) -> bool {
    let id = u16::try_from(uuid::Uuid::new_v4().as_u128() & 0xffff).unwrap_or(0);
    let Some(wire) = query_wire(zone, SOA_TYPE, id, true) else {
        return false;
    };
    // One dual-stack ephemeral socket; IPv4 targets go out as v4-mapped.
    let Ok(socket) = tokio::net::UdpSocket::bind("[::]:0").await else {
        return false;
    };
    let mut sent = false;
    for target in probe {
        let to = match target.ip() {
            IpAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), target.port()),
            IpAddr::V6(_) => *target,
        };
        // Unreachable families (no route) are simply not probed.
        if socket.send_to(&wire, to).await.is_ok() {
            sent = true;
        }
    }
    if !sent {
        return false;
    }
    let wait = async {
        let mut buf = [0_u8; 512];
        loop {
            // A transient receive error (e.g. a reported ICMP unreachable) is
            // not an answer, nor is a datagram that is not a reply to THIS
            // query from one of the probe targets (stray or spoofed traffic):
            // keep listening until the deadline.
            if let Ok((len, from)) = socket.recv_from(&mut buf).await {
                if is_probe_reply(buf.get(..len).unwrap_or_default(), from, id, probe) {
                    return true;
                }
            }
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, wait)
        .await
        .unwrap_or(false)
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
            let method = self.method(&zone.name).await;
            *self
                .last_method
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(method);
            if method == Method::Doh {
                return self.confirm_via_doh(fqdn, value, until).await;
            }
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

    fn propagation_detail(&self) -> Option<String> {
        let method = *self
            .last_method
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last_doh = self
            .last_doh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or_else(String::new, |o| format!("; last DoH answer: {o}"));
        method.map(|method| match (method, self.confirm) {
            (Method::Doh, DnsConfirm::Auto) => format!(
                "TXT not seen via DNS over HTTPS (cloudflare-dns.com) before the deadline \
                 (port 53 intercepted){last_doh}"
            ),
            (Method::Doh, _) => format!(
                "TXT not seen via DNS over HTTPS (cloudflare-dns.com) before the deadline \
                 (confirm = \"doh\"){last_doh}"
            ),
            (Method::Authoritative, _) => {
                "TXT not seen at every authoritative nameserver (port 53) before the deadline; \
                 if this network intercepts DNS, set [network.tls.dns] confirm = \"doh\""
                    .to_owned()
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
    fn txt_answers_match_byte_for_byte_only() {
        use hickory_resolver::proto::op::{Message, MessageType, OpCode};
        use hickory_resolver::proto::rr::rdata::TXT;
        use hickory_resolver::proto::rr::{Name, RData, Record};
        let query = query_wire("_acme-challenge.a.example.org", TXT_TYPE, 7, true).unwrap();
        let parsed = Message::from_vec(&query).unwrap();
        assert!(parsed.metadata.recursion_desired);
        assert_eq!(parsed.queries.len(), 1);
        let answer = |parts: Vec<String>| {
            let mut m = Message::new(7, MessageType::Response, OpCode::Query);
            m.add_answer(Record::from_rdata(
                Name::from_ascii("_acme-challenge.a.example.org.").unwrap(),
                60,
                RData::TXT(TXT::new(parts)),
            ));
            m.to_vec().unwrap()
        };
        assert!(txt_answer_matches(&answer(vec!["abc".into()]), "abc"));
        assert!(txt_answer_matches(
            &answer(vec!["ab".into(), "c".into()]),
            "abc"
        ));
        assert!(!txt_answer_matches(&answer(vec!["abd".into()]), "abc"));
        assert!(!txt_answer_matches(&answer(vec!["abc ".into()]), "abc"));
        assert!(!txt_answer_matches(b"not dns", "abc"));
        assert!(query_wire("bad..name", TXT_TYPE, 0, true).is_none());
    }

    #[test]
    fn doh_answers_are_classified_with_bounded_reasons() {
        use hickory_resolver::proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
        use hickory_resolver::proto::rr::{Name, RecordType};
        let fqdn = "_acme-challenge.a.example.org";
        let reply = |id: u16, qname: &str, rcode: ResponseCode| {
            let mut m = Message::new(id, MessageType::Response, OpCode::Query);
            m.metadata.response_code = rcode;
            m.add_query(Query::query(
                Name::from_ascii(qname).unwrap(),
                RecordType::TXT,
            ));
            m.to_vec().unwrap()
        };
        let nx = reply(0, "_ACME-challenge.a.example.org.", ResponseCode::NXDomain);
        let outcome = classify_doh_answer_for(&nx, fqdn, "secret-value");
        assert_eq!(
            outcome,
            DohOutcome::NotSeen {
                rcode: 3,
                answers: 0,
                txt: 0
            }
        );
        let text = outcome.to_string();
        assert!(text.contains("NXDOMAIN"), "{text}");
        assert!(!text.contains("secret-value"), "{text}");
        assert_eq!(
            classify_doh_answer_for(
                &reply(9, "_acme-challenge.a.example.org.", ResponseCode::NoError),
                fqdn,
                "v"
            ),
            DohOutcome::Mismatched
        );
        assert_eq!(
            classify_doh_answer_for(
                &reply(0, "other.example.org.", ResponseCode::NoError),
                fqdn,
                "v"
            ),
            DohOutcome::Mismatched
        );
        assert_eq!(
            classify_doh_answer_for(b"junk", fqdn, "v"),
            DohOutcome::Unparsable
        );
    }

    #[test]
    fn a_doh_txt_counts_only_at_the_queried_owner() {
        use hickory_resolver::proto::op::{Message, MessageType, OpCode, Query};
        use hickory_resolver::proto::rr::rdata::TXT;
        use hickory_resolver::proto::rr::{Name, RData, Record, RecordType};
        let fqdn = "_acme-challenge.a.example.org";
        let reply = |owner: &str| {
            let mut m = Message::new(0, MessageType::Response, OpCode::Query);
            m.add_query(Query::query(
                Name::from_ascii(format!("{fqdn}.")).unwrap(),
                RecordType::TXT,
            ));
            m.add_answer(Record::from_rdata(
                Name::from_ascii(owner).unwrap(),
                60,
                RData::TXT(TXT::new(vec!["the-value".into()])),
            ));
            m.to_vec().unwrap()
        };
        assert_eq!(
            classify_doh_answer_for(&reply("_ACME-Challenge.a.example.org."), fqdn, "the-value"),
            DohOutcome::Seen
        );
        assert_eq!(
            classify_doh_answer_for(&reply("elsewhere.example.org."), fqdn, "the-value"),
            DohOutcome::NotSeen {
                rcode: 0,
                answers: 1,
                txt: 1
            }
        );
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
            confirm: DnsConfirm::Auto,
        };
        assert_eq!(
            CloudflareCredential::from_config(&cfg, &lookup).unwrap_err(),
            AcmeError::Credential
        );
    }
}
