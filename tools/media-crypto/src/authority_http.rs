//! Fail-closed HTTPS adapter for the frozen publisher authority contract,
//! including the exclusive publisher lease amendment.
//!
//! This adapter never generates a root. The resolved organisation-scoped CSK
//! remains in an injected credential holder, never in a URL, log or Debug
//! representation. The production transport verifies TLS and follows no
//! redirect. No HTTP operation is invoked by constructing the provider.
//!
//! The lease amendment (frozen contract, not deployed when this was written) adds:
//!
//! * `POST .../publisher/lease/acquire` `{machineId,bootNonce}` -> `201`
//!   `{generation,leaseToken,leaseExpiresAtMs,notBeforeMs}`: the token is handed
//!   out **once**, here, and is canonical base64url of 32 bytes;
//! * `POST .../publisher/lease/renew`
//!   `{machineId,bootNonce,leaseToken,generation}` -> `200` with **metadata
//!   only** `{generation,leaseExpiresAtMs,notBeforeMs}`: a renewal never rotates
//!   the token, so the held token is retained and the answer must not carry a
//!   different one (a different `leaseToken` is refused, never adopted). A token
//!   that is present and byte-identical to the held one is tolerated for a
//!   loose-but-canonical server, but it is never the source of the retained
//!   value;
//! * `POST .../publisher/lease/release`
//!   `{machineId,bootNonce,leaseToken,generation}` -> `200`;
//! * `GET .../publisher/current-epoch?...` requires `Exchange-Publisher-Lease`
//!   and `Exchange-Publisher-Boot-Nonce`; a request without a live lease is
//!   refused with `409` by the server, and is never attempted here without one.
//!
//! Local consequences, all fail-closed:
//!
//! * The boot nonce is 16 CSPRNG bytes (canonical base64url, 22 chars) created
//!   per process and never persisted or reused. A lost lease token is not
//!   re-minted: the process waits for the server to expire the old lease and for
//!   a new boot nonce.
//! * The local seal deadline is monotonic and conservatively derived from the
//!   server expiry minus clock uncertainty, never the server value alone. The
//!   fence gates on both the local wall clock and that monotonic deadline, and
//!   on the server's `notBeforeMs` plus uncertainty.
//! * [`PublisherLease`] is the [`LeaseFence`] implementation handed to the
//!   crypto owner's guard: an expired, released, revoked or failed leased
//!   process stops sealing and granting even though its root is still in memory.
//! * Nothing here logs, formats or URL-encodes the CSK or the lease token; the
//!   token lives in `Zeroizing` storage and the response types have no `Debug`.
//!
//! # Opaque HTTP resource ids are not the crypto identity
//!
//! The authority addresses a publisher with **opaque strings** (an organisation
//! id like `org_<uuid>`, an exact version id string, and an arbitrary machine id
//! such as `mch_...`), while the crypto layer binds raw 16-byte identities. This
//! adapter keeps the two strictly separate: [`HttpResourceIds`] carries the
//! exact strings used in the path, query and body, and [`CryptoBinding`] carries
//! the explicitly registered raw identities used for the
//! [`AuthorityEpochRoot`](crate::AuthorityEpochRoot) context. An id is never
//! stripped, parsed, hashed or coerced into the other form, and there is no
//! constructor that derives one from the other. The authority's own CSK →
//! (opaque ids, crypto binding) mapping is checked by the authority at
//! registration; a missing or mismatched mapping fails closed here, because the
//! adapter cannot invent a binding it was not given.
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine as _;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Url;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::authority::{
    AuthorityEpochRoot, AuthorityRootProvider, EpochRootRequest, LeaseFence, LeasePermit,
};
use crate::{Error, Result, EPOCH_MS};

const MAX_RESPONSE_BYTES: usize = 2048;
const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;
/// Upper bound on one opaque resource id, so an unbounded string cannot be
/// smuggled into a URL, a JSON body or an identity comparison.
const MAX_ID_CHARS: usize = 256;

/// Server lease time-to-live in the frozen contract (diagnostics only: the
/// client never assumes it, it uses the server's own expiry).
pub const LEASE_TTL_MS: u64 = 10_000;
/// The frozen contract's renew-by window. A runtime must renew well inside it.
pub const LEASE_RENEW_BY_MS: u64 = 5_000;
/// Canonical base64url of 32 random bytes, unpadded.
const LEASE_TOKEN_CHARS: usize = 43;
/// Default allowance for this host's clock differing from the authority's when
/// deriving the local seal deadline and the local `notBefore` instant.
pub const DEFAULT_CLOCK_UNCERTAINTY_MS: u64 = 1_000;
/// Default additional margin covering the time between taking a permit and the
/// completed seal.
pub const DEFAULT_SCHEDULER_MARGIN_MS: u64 = 250;
const MAX_CLOCK_UNCERTAINTY_MS: u64 = 2_000;
const MAX_SCHEDULER_MARGIN_MS: u64 = 1_000;

/// The opaque HTTP resource identifiers the authority addresses a publisher
/// with, kept byte-for-byte exactly as issued.
///
/// These are **not** the crypto identity and are never converted to or from it.
/// Each value must be non-empty, bounded ([`MAX_ID_CHARS`]) and free of ASCII
/// control characters; anything else (including spaces, `/`, `?`, `#`, `%` and
/// non-ASCII) is preserved exactly and percent-encoded by the URL API when it is
/// placed in a path segment or query parameter.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResourceIds {
    org_id: String,
    version_id: String,
    machine_id: String,
}
impl HttpResourceIds {
    /// Validate and keep the three opaque ids exactly as given.
    ///
    /// # Errors
    /// [`Error::Denied`] for an empty, over-long or control-character-bearing id.
    pub fn new(
        org_id: impl Into<String>,
        version_id: impl Into<String>,
        machine_id: impl Into<String>,
    ) -> Result<Self> {
        let (org_id, version_id, machine_id) =
            (org_id.into(), version_id.into(), machine_id.into());
        for value in [&org_id, &version_id, &machine_id] {
            if !valid_id(value) {
                return Err(Error::Denied);
            }
        }
        Ok(Self {
            org_id,
            version_id,
            machine_id,
        })
    }

    /// The organisation id exactly as issued (for example `org_<uuid>`).
    #[must_use]
    pub fn org_id(&self) -> &str {
        &self.org_id
    }
    /// The version id exactly as issued.
    #[must_use]
    pub fn version_id(&self) -> &str {
        &self.version_id
    }
    /// The publisher machine id exactly as issued (for example `mch_...`).
    #[must_use]
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }
}
impl std::fmt::Debug for HttpResourceIds {
    /// Hand-written so the ids stay readable for diagnostics: they are
    /// identifiers, not secrets (they travel in the URL).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpResourceIds")
            .field("org_id", &self.org_id)
            .field("version_id", &self.version_id)
            .field("machine_id", &self.machine_id)
            .finish()
    }
}

/// The canonical raw 16-byte crypto identity the authority registration binds
/// the opaque [`HttpResourceIds`] to.
///
/// It is supplied explicitly by the caller from the registration result and is
/// **never** derived from the opaque strings. [`HttpAuthorityRootProvider`]
/// checks the authenticated response against the opaque ids and builds the
/// crypto context from this binding only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CryptoBinding {
    /// Canonical organisation identity.
    pub org: [u8; 16],
    /// Canonical version identity.
    pub version: [u8; 16],
    /// Canonical publisher-machine identity.
    pub publisher_machine: [u8; 16],
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_CHARS && !value.bytes().any(|b| b.is_ascii_control())
}

/// A resolved organisation-scoped credential, not a personal token or JWT.
/// The caller is responsible for resolving a secret reference securely.
#[derive(Clone)]
pub struct OrganisationCsk(Zeroizing<String>);
impl OrganisationCsk {
    /// Accept only a non-empty CSK without header control characters.
    pub fn new(token: String) -> Result<Self> {
        if token.is_empty() || token.len() > 4096 || token.bytes().any(|b| b.is_ascii_control()) {
            return Err(Error::Denied);
        }
        Ok(Self(Zeroizing::new(token)))
    }
}
impl std::fmt::Debug for OrganisationCsk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OrganisationCsk(<redacted>)")
    }
}

/// The one bounded HTTP exchange shape. `body=None` means absolutely no GET body.
pub struct AuthorityHttpRequest {
    /// HTTP verb.
    pub method: &'static str,
    /// HTTPS URL. Never contains the credential, lease token or epoch root.
    pub url: String,
    /// Optional JSON request body.
    pub body: Option<Vec<u8>>,
    /// Lease headers; secret values must never be logged or put in URLs.
    pub lease_headers: Option<LeaseHeaders>,
}
/// Secret lease headers on the current-root request. No Debug to prevent
/// accidental token publication in diagnostics.
pub struct LeaseHeaders {
    /// Server-issued bearer token (base64url, held in zeroizing storage).
    pub token: Zeroizing<String>,
    /// This process's fresh boot nonce (base64url). Not secret, but never
    /// persisted: reusing it across processes is what the amendment forbids.
    pub boot_nonce: String,
}

/// Response bytes from one authority request. Must not be logged: an epoch
/// response contains a root key. Debug is intentionally not implemented.
pub struct AuthorityHttpResponse {
    /// HTTP status.
    pub status: u16,
    /// Bounded JSON bytes, potentially secret.
    pub body: Vec<u8>,
}
/// Synchronous control-plane transport, injected for deterministic tests.
/// Never call it from an output tick.
pub trait AuthorityHttpTransport: Send + Sync {
    /// Fetch a bounded binary grant. Existing JSON-only transports fail closed.
    fn send_grant(
        &self,
        _request: AuthorityHttpRequest,
        _credential: &OrganisationCsk,
    ) -> Result<GrantHttpResponse> {
        Err(Error::Denied)
    }

    /// Make exactly one request; fail closed on redirects/timeouts/errors.
    fn send(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
    ) -> Result<AuthorityHttpResponse>;
}

/// Redacted, process-wide record of recent HTTP exchanges for failure reports:
/// method, route template, status (0 = transport error/timeout) and ms since
/// start. Never a token, id, query, body or key.
static STATUS_LOG: Mutex<Vec<(u64, &'static str, String, u16)>> = Mutex::new(Vec::new());
static STATUS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
const STATUS_LOG_CAP: usize = 128;

/// Pin the "ms since start" origin (idempotent; first call wins).
pub fn status_log_start() {
    STATUS_START.get_or_init(std::time::Instant::now);
}

/// Replace every opaque id segment by a placeholder and drop the query/fragment.
pub fn route_template(url: &str) -> String {
    let path = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest.find('/').map_or("", |i| &rest[i..]));
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut out = String::new();
    let mut placeholder: Option<&str> = None;
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        if let Some(name) = placeholder.take() {
            out.push_str(name);
            continue;
        }
        out.push_str(segment);
        placeholder = match segment {
            "organisations" => Some("{org}"),
            "bookings" => Some("{booking}"),
            "versions" => Some("{version}"),
            "stories" => Some("{story}"),
            _ => None,
        };
    }
    out
}

fn record_status(method: &'static str, url: &str, status: u16) {
    let ms = STATUS_START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    let mut log = lock(&STATUS_LOG);
    if log.len() >= STATUS_LOG_CAP {
        log.remove(0);
    }
    log.push((ms, method, route_template(url), status));
}

/// The recorded sequence, oldest first, one redacted line per exchange.
pub fn status_sequence() -> Vec<String> {
    lock(&STATUS_LOG)
        .iter()
        .map(|(ms, method, route, status)| format!("+{ms}ms {method} {route} -> {status}"))
        .collect()
}

/// Production HTTPS transport: system-trusted TLS, no redirect, bounded timeout
/// and response body. It neither logs nor exposes the root in Debug.
#[derive(Clone)]
pub struct VerifiedHttpsTransport(Client);
impl VerifiedHttpsTransport {
    /// Construct the production client with its unchanged default trust roots.
    pub fn new() -> Result<Self> {
        Self::build(None)
    }

    /// Construct a test-harness client with one explicitly supplied additional CA.
    ///
    /// This adds trust only to this client. Normal certificate chain, expiry and
    /// DNS/IP SAN verification remain enabled. Never install a harness CA into
    /// ambient process or operating-system trust.
    pub fn from_trusted_ca_pem(pem: &[u8]) -> Result<Self> {
        let mut certificates =
            reqwest::Certificate::from_pem_bundle(pem).map_err(|_| Error::Invalid)?;
        if certificates.len() != 1 {
            return Err(Error::Invalid);
        }
        Self::build(certificates.pop())
    }

    /// Harness-only one-second exchanges leave room for a <=5s lease cadence.
    pub fn from_harness_ca_pem(pem: &[u8]) -> Result<Self> {
        let mut certificates =
            reqwest::Certificate::from_pem_bundle(pem).map_err(|_| Error::Invalid)?;
        if certificates.len() != 1 {
            return Err(Error::Invalid);
        }
        Self::build_timeout(certificates.pop(), Duration::from_secs(1))
    }

    fn build(additional_ca: Option<reqwest::Certificate>) -> Result<Self> {
        Self::build_timeout(additional_ca, TIMEOUT)
    }

    fn build_timeout(
        additional_ca: Option<reqwest::Certificate>,
        timeout: Duration,
    ) -> Result<Self> {
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Explicit trust must not silently select a different TLS backend.
            .use_rustls_tls();
        if let Some(ca) = additional_ca {
            builder = builder.add_root_certificate(ca);
        }
        let client = builder
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .map_err(|_| Error::Denied)?;
        Ok(Self(client))
    }
}
/// Bounded secret binary response, deliberately without Debug.
pub struct GrantHttpResponse {
    pub status: u16,
    pub body: Zeroizing<Vec<u8>>,
    pub content_type: String,
    pub cache_control: String,
    pub content_length: Option<u64>,
}
impl AuthorityHttpTransport for VerifiedHttpsTransport {
    fn send_grant(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
    ) -> Result<GrantHttpResponse> {
        self.exchange(request, credential, 264)
    }
    fn send(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
    ) -> Result<AuthorityHttpResponse> {
        let response = self.exchange(request, credential, MAX_RESPONSE_BYTES)?;
        Ok(AuthorityHttpResponse {
            status: response.status,
            body: response.body.to_vec(),
        })
    }
}
impl VerifiedHttpsTransport {
    fn exchange(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
        limit: usize,
    ) -> Result<GrantHttpResponse> {
        if !request.url.starts_with("https://") {
            return Err(Error::Denied);
        }
        let method = match request.method {
            "POST" => reqwest::Method::POST,
            "GET" if request.body.is_none() => reqwest::Method::GET,
            _ => return Err(Error::Invalid),
        };
        let method_name = request.method;
        let request_url = request.url.clone();
        let mut outgoing = self
            .0
            .request(method, &request.url)
            .header(AUTHORIZATION, format!("Bearer {}", credential.0.as_str()));
        if let Some(headers) = request.lease_headers {
            outgoing = outgoing
                .header("Exchange-Publisher-Lease", headers.token.as_str())
                .header("Exchange-Publisher-Boot-Nonce", headers.boot_nonce);
        }
        if let Some(body) = request.body {
            outgoing = outgoing.header(CONTENT_TYPE, "application/json").body(body);
        }
        let sent = outgoing.send();
        record_status(
            method_name,
            &request_url,
            sent.as_ref().map_or(0, |r| r.status().as_u16()),
        );
        let response = sent.map_err(|_| Error::Denied)?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            return Err(Error::Denied);
        }
        if limit == 264 && status != 200 {
            // A grant denial is a plain no-oracle status (the epoch-tail and
            // booking-lifetime 404s among them). Surface the status so the caller
            // can apply its bounded policy; the body is never read or kept.
            return Ok(GrantHttpResponse {
                status,
                body: Zeroizing::new(Vec::new()),
                content_type: String::new(),
                cache_control: String::new(),
                content_length: None,
            });
        }
        let content_length = if limit == 264 {
            Some(grant_content_length(status, response.headers())?)
        } else {
            response.content_length()
        };
        if content_length.is_some_and(|n| n > limit as u64) {
            return Err(Error::Capacity);
        }
        // Reject duplicate headers rather than selecting an ambiguous value.
        let header = |name: &str| -> Result<String> {
            let mut values = response.headers().get_all(name).iter();
            let value = values
                .next()
                .map(|v| v.to_str().map(str::to_owned))
                .transpose()
                .map_err(|_| Error::Denied)?
                .unwrap_or_default();
            if values.next().is_some() {
                return Err(Error::Denied);
            }
            Ok(value)
        };
        let content_type = header("content-type")?;
        let cache_control = header("cache-control")?;
        let mut body = Zeroizing::new(Vec::new());
        use std::io::Read;
        response
            .take((limit + 1) as u64)
            .read_to_end(&mut body)
            .map_err(|_| Error::Denied)?;
        if body.len() > limit {
            return Err(Error::Capacity);
        }
        Ok(GrantHttpResponse {
            status,
            body,
            content_type,
            cache_control,
            content_length,
        })
    }
}

// Require an unambiguous frozen binary response framing. Do not use the
// decoded body's size hint as a substitute for the wire Content-Length.
fn grant_content_length(status: u16, headers: &reqwest::header::HeaderMap) -> Result<u64> {
    // Error responses need not carry binary grant metadata or a grant body.
    if status != 200 {
        return Err(Error::Denied);
    }
    if headers.contains_key("transfer-encoding") || headers.contains_key("content-encoding") {
        return Err(Error::Invalid);
    }
    let mut lengths = headers.get_all("content-length").iter();
    if lengths.next().and_then(|v| v.to_str().ok()) != Some("264") || lengths.next().is_some() {
        return Err(Error::Invalid);
    }
    Ok(264)
}

/// The two clock samples every lease decision is derived from. Both are
/// injectable so tests are deterministic; a protected deployment must use the
/// default system clocks.
pub struct LeaseClock {
    /// Unix milliseconds.
    pub wall_ms: Arc<dyn Fn() -> Result<u64> + Send + Sync>,
    /// Monotonic milliseconds in any fixed domain: comparisons are relative.
    pub monotonic_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
}
impl LeaseClock {
    /// System wall clock plus a process-monotonic elapsed clock.
    #[must_use]
    pub fn system() -> Self {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        let start = *START.get_or_init(std::time::Instant::now);
        Self {
            wall_ms: Arc::new(|| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| Error::Invalid)?;
                u64::try_from(now.as_millis()).map_err(|_| Error::Invalid)
            }),
            monotonic_ms: Arc::new(move || {
                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
            }),
        }
    }
}
impl Default for LeaseClock {
    fn default() -> Self {
        Self::system()
    }
}

/// The wire answer to an acquire or a renew, as validated by this adapter.
///
/// `token` is `Some` only for an acquire, where the contract hands the token out
/// once, or for a renewal whose (non-normative) answer echoed the exact token
/// already held. A metadata-only renewal leaves it `None`, and the caller keeps
/// the token it already has: a renewal never rotates the token.
struct LeaseGrant {
    generation: u32,
    token: Option<Zeroizing<String>>,
    expires_at_ms: u64,
    not_before_ms: u64,
}

/// Held lease state. Never formatted; the token is zeroized on drop, release
/// and takeover.
struct LeaseState {
    generation: u32,
    token: Zeroizing<String>,
    /// Server wall-clock expiry of the lease.
    expires_at_ms: u64,
    /// Server instant from which the *new* root may be used.
    not_before_ms: u64,
    /// Conservative local monotonic seal deadline.
    deadline_monotonic_ms: u64,
    /// Conservative local monotonic instant at which sealing becomes legal.
    not_before_monotonic_ms: u64,
    /// Latched on release/revocation: a lost lease is never resumed locally.
    released: bool,
    /// Bumped by every `release`/`revoke`. A wire call that started under an
    /// older value must not install anything when it lands (R1: a late renew
    /// or acquire answer can never resurrect a lease latched meanwhile).
    latch_epoch: u64,
}
impl Default for LeaseState {
    fn default() -> Self {
        Self {
            generation: 0,
            token: Zeroizing::new(String::new()),
            expires_at_ms: 0,
            not_before_ms: 0,
            deadline_monotonic_ms: 0,
            not_before_monotonic_ms: 0,
            released: true,
            latch_epoch: 0,
        }
    }
}

/// Everything a lease call needs to build its request, cloned so no state lock
/// is held across the network exchange.
struct LeaseEndpoint {
    /// Origin-only URL; every request is built from it with the URL API.
    base: Url,
    /// The exact opaque ids used in the path, query and body.
    ids: HttpResourceIds,
    /// The registered crypto binding this lease is paired with. Not used to
    /// build a request; the provider refuses to pair a lease whose binding or
    /// ids differ from its own, so a mismatched registration fails closed.
    binding: CryptoBinding,
    credential: OrganisationCsk,
    transport: Arc<dyn AuthorityHttpTransport>,
    clock: LeaseClock,
    uncertainty_ms: u64,
    scheduler_margin_ms: u64,
    /// HTTP status of the last lease answer (0 = no answer: transport error/timeout).
    last_status: std::sync::atomic::AtomicU16,
}

/// The exclusive publisher lease client and its local [`LeaseFence`].
///
/// One process holds one instance (an `Arc` shared with the
/// [`AuthorityRootGuard`]). Mutating lease calls are serialized; the fence
/// itself takes one short mutex and performs no I/O, so it never blocks a data
/// path on the network.
pub struct PublisherLease {
    /// Fresh per process. Never persisted, never regenerated on retry.
    boot_nonce: String,
    endpoint: Arc<LeaseEndpoint>,
    state: Mutex<LeaseState>,
    /// Serializes acquire/renew/release so wire calls cannot interleave.
    calls: Mutex<()>,
    /// Static, non-secret classification of the last failed lease call, for
    /// operator diagnostics only. Never carries a token, body or status text.
    last_failure: Mutex<&'static str>,
    /// Status of the authority answer that revoked this lease (0 = none/local).
    revoked_by: Mutex<u16>,
}

impl PublisherLease {
    /// Bind a lease client to an origin, the authority's opaque resource ids and
    /// the crypto binding registered for them. No HTTP happens here. The boot
    /// nonce is drawn once, from the OS CSPRNG.
    pub fn new(
        origin: &str,
        ids: HttpResourceIds,
        binding: CryptoBinding,
        credential: OrganisationCsk,
    ) -> Result<Self> {
        Self::with_transport(
            origin,
            ids,
            binding,
            credential,
            Arc::new(VerifiedHttpsTransport::new()?),
            LeaseClock::system(),
        )
    }

    /// Inject a transport and clocks (tests, or an audited runtime transport).
    pub fn with_transport(
        origin: &str,
        ids: HttpResourceIds,
        binding: CryptoBinding,
        credential: OrganisationCsk,
        transport: Arc<dyn AuthorityHttpTransport>,
        clock: LeaseClock,
    ) -> Result<Self> {
        let base = validated_origin(origin)?;
        let mut boot_nonce = [0u8; 16];
        getrandom::getrandom(&mut boot_nonce).map_err(|_| Error::Crypto)?;
        Ok(Self {
            boot_nonce: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(boot_nonce),
            endpoint: Arc::new(LeaseEndpoint {
                base,
                ids,
                binding,
                credential,
                transport,
                clock,
                uncertainty_ms: DEFAULT_CLOCK_UNCERTAINTY_MS,
                scheduler_margin_ms: DEFAULT_SCHEDULER_MARGIN_MS,
                last_status: std::sync::atomic::AtomicU16::new(0),
            }),
            state: Mutex::new(LeaseState::default()),
            calls: Mutex::new(()),
            last_failure: Mutex::new(""),
            revoked_by: Mutex::new(0),
        })
    }

    /// The exact opaque ids this lease addresses, for the provider's pairing
    /// check. Never used to derive a crypto identity.
    #[must_use]
    pub fn ids(&self) -> &HttpResourceIds {
        &self.endpoint.ids
    }

    /// The crypto binding registered with those ids, for the provider's pairing
    /// check. Never derived from the ids.
    #[must_use]
    pub fn binding(&self) -> CryptoBinding {
        self.endpoint.binding
    }

    /// Override the clock-uncertainty and scheduler margins. Bounded: an
    /// unbounded value would let a caller keep sealing long past the server's
    /// expiry, and a zero uncertainty is not conservative.
    #[must_use]
    pub fn with_margins(mut self, uncertainty_ms: u64, scheduler_margin_ms: u64) -> Self {
        let endpoint =
            Arc::get_mut(&mut self.endpoint).expect("no other owner during construction");
        endpoint.uncertainty_ms = uncertainty_ms.min(MAX_CLOCK_UNCERTAINTY_MS);
        endpoint.scheduler_margin_ms = scheduler_margin_ms.min(MAX_SCHEDULER_MARGIN_MS);
        self
    }

    /// This process's boot nonce (canonical base64url of 16 CSPRNG bytes).
    #[must_use]
    pub fn boot_nonce(&self) -> &str {
        &self.boot_nonce
    }

    /// The lease generation currently held, if any.
    #[must_use]
    pub fn generation(&self) -> Option<u32> {
        let state = lock(&self.state);
        (!state.released).then_some(state.generation)
    }

    /// Fail closed *now*, from outside a lease call.
    ///
    /// Used when the authority tells us this lease is no longer valid — a
    /// `lease_required` / stale-lease rejection on the root route, or any other
    /// authoritative "this lease is gone". An authority-expired lease is not a
    /// local failure to retry: this process must wait for the server to expire
    /// the record and for a new boot nonce. Later `renew`, `release` and root
    /// requests are refused; only a fresh process may acquire again.
    pub fn revoke(&self) {
        self.revoke_with_status(0);
    }

    /// [`revoke`](Self::revoke), recording the authoritative non-secret HTTP
    /// status that revoked the lease (e.g. the root route's 404/409), so an
    /// operator can tell an authority revocation from a local failure.
    pub fn revoke_with_status(&self, status: u16) {
        *lock(&self.revoked_by) = status;
        let mut state = lock(&self.state);
        state.released = true;
        state.token = Zeroizing::new(String::new());
        state.latch_epoch = state.latch_epoch.wrapping_add(1);
    }

    /// Whether a renewal should be issued now: the contract's renew-by window
    /// has elapsed since the last successful acquire/renew. The runtime's lease
    /// thread polls this; it is never true on the data plane.
    #[must_use]
    pub fn needs_renewal(&self, monotonic_ms: u64) -> bool {
        let state = lock(&self.state);
        if state.released {
            return true;
        }
        state.deadline_monotonic_ms.saturating_sub(monotonic_ms) <= LEASE_RENEW_BY_MS
    }

    /// Acquire the exclusive lease. A `409` (another host holds it, or this boot
    /// nonce already has an active lease whose token was lost) is returned as
    /// [`Error::Denied`] and is never retried here: minting a second token
    /// implicitly is exactly what the amendment forbids.
    pub fn acquire(&self) -> Result<u32> {
        self.refresh(false)
    }

    /// Renew the held lease. Requires a token acquired by
    /// [`acquire`](Self::acquire); a process that lost its token must fail
    /// closed and wait for a new boot nonce.
    pub fn renew(&self) -> Result<u32> {
        self.refresh(true)
    }

    /// Give the lease back and stop sealing locally. Idempotent; the token is
    /// zeroized.
    pub fn release(&self) -> Result<()> {
        // Latched locally *before* waiting for the wire serializer: a renew that
        // is in flight must not delay the local stop, and its late answer finds
        // the bumped latch epoch and installs nothing.
        let body = {
            let mut state = lock(&self.state);
            if state.released {
                // Still bump the latch: an acquire may be in flight (acquire only
                // starts while released), and its answer must not install a lease
                // after this release() returned.
                state.latch_epoch = state.latch_epoch.wrapping_add(1);
                return Ok(());
            }
            let body = json!({
                "machineId": self.endpoint.ids.machine_id(),
                "bootNonce": self.boot_nonce,
                "leaseToken": state.token.as_str(),
                "generation": state.generation,
            });
            // Latched before the wire call: even if the server never answers,
            // this process does not seal again under this lease.
            state.released = true;
            state.token = Zeroizing::new(String::new());
            state.latch_epoch = state.latch_epoch.wrapping_add(1);
            body
        };
        // The release is sent after any in-flight lease call, never interleaved.
        let _serialized = lock(&self.calls);
        let reply = send_lease(&self.endpoint, "release", Some(body), 200);
        // Whatever the answer, the local lease stays released.
        reply.map(|_| ())
    }

    /// The wire call plus the state transition. Serialized by `calls`, which is
    /// the only lock held across I/O, and only from a control-plane thread.
    fn refresh(&self, renewing: bool) -> Result<u32> {
        let result = self.refresh_inner(renewing);
        if let Err(error) = &result {
            *lock(&self.last_failure) = match error {
                Error::Denied => {
                    "denied (non-success status, transport error/timeout, or local latch)"
                }
                Error::Expired => "answer expired on arrival (late or slow answer)",
                Error::Replay => "generation went backwards",
                Error::Invalid => "malformed answer",
                Error::Capacity => "oversized answer",
                Error::Crypto => "local randomness/crypto failure",
                Error::GenerationOverflow => "generation overflow",
            };
        }
        result
    }

    /// Status of the authority answer that revoked this lease via
    /// [`revoke_with_status`](Self::revoke_with_status) (0 = not revoked that way).
    #[must_use]
    pub fn revoked_by(&self) -> u16 {
        *lock(&self.revoked_by)
    }

    /// The static classification of the last failed acquire/renew, if any, and
    /// the HTTP status of the last lease answer (0 = none: transport error or
    /// timeout). Non-secret: no token, body or header is ever retained here.
    #[must_use]
    pub fn last_failure(&self) -> (&'static str, u16) {
        (
            *lock(&self.last_failure),
            self.endpoint
                .last_status
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn refresh_inner(&self, renewing: bool) -> Result<u32> {
        let _serialized = lock(&self.calls);
        // What this call renews, and its body, computed under a short lock.
        let (slot, body, latch) = {
            let state = lock(&self.state);
            if !renewing && !state.released {
                // This process still holds a lease. The contract answers a live
                // re-acquire with `409`, so never send it: a second token would
                // be minted only by the rejected call, and the held token is
                // retained for `renew`.
                return Err(Error::Denied);
            }
            if renewing {
                if state.token.is_empty() {
                    // Nothing to renew: the token was lost or the lease was
                    // revoked. Waiting for a new boot nonce is the only
                    // sanctioned recovery.
                    return Err(Error::Denied);
                }
                (
                    state.generation,
                    Some(json!({
                        "machineId": self.endpoint.ids.machine_id(),
                        "bootNonce": self.boot_nonce,
                        "leaseToken": state.token.as_str(),
                        "generation": state.generation,
                    })),
                    state.latch_epoch,
                )
            } else {
                (
                    0,
                    Some(json!({
                        "machineId": self.endpoint.ids.machine_id(),
                        "bootNonce": self.boot_nonce,
                    })),
                    state.latch_epoch,
                )
            }
        };
        let route = if renewing { "renew" } else { "acquire" };
        let expected = if renewing { 200 } else { 201 };
        let reply = send_lease(&self.endpoint, route, body, expected)?;
        let mut grant = parse_lease(&reply, &self.endpoint.ids, &self.boot_nonce, renewing)?;

        // The token transition is decided against live state, not against the
        // pre-call snapshot: `release`/`revoke` take only `state`, so they may run
        // while this call is in flight.
        {
            let mut state = lock(&self.state);
            if state.latch_epoch != latch {
                return Err(Error::Denied);
            }
            if renewing {
                if state.released || state.token.is_empty() {
                    // Released or latched from another thread while this renew
                    // was in flight: the lease stays dead and no token is
                    // installed.
                    return Err(Error::Denied);
                }
                // A renewal never rotates the token. A metadata-only answer keeps
                // the held token; an answer that carries the exact held token is
                // tolerated but changes nothing. Any other token is the authority
                // equivocating on this lease (R2): fail closed *now*. The fence is
                // latched exactly like a root-route 409/404, the held token is
                // zeroized, and the rotated value is never installed. Only a new
                // process with a new boot nonce may acquire again.
                if let Some(renewed) = &grant.token {
                    if renewed.as_str() != state.token.as_str() {
                        state.released = true;
                        state.token = Zeroizing::new(String::new());
                        state.latch_epoch = state.latch_epoch.wrapping_add(1);
                        return Err(Error::Denied);
                    }
                }
            }
        }

        let (wall, monotonic) = self.sample()?;
        let (deadline, not_before) = conservatively_bound(&self.endpoint, &grant, wall, monotonic)?;

        let mut state = lock(&self.state);
        // R1: re-checked inside the final mutation lock. A `release`/`revoke`
        // that ran while this call was on the wire or being bounded wins; the
        // answer is discarded (any acquired token is zeroized on drop).
        if state.latch_epoch != latch || (renewing && (state.released || state.token.is_empty())) {
            return Err(Error::Denied);
        }
        if renewing {
            // A renewal never moves the generation backwards; a lower value is
            // the authority equivocating on this identity.
            if grant.generation < slot {
                return Err(Error::Replay);
            }
            state.generation = grant.generation;
            // `notBefore` is monotonic: a renewal that repeats the current value
            // keeps the already-elapsed local instant, so a legitimate renewal
            // never re-closes sealing after a takeover's wait is over.
            if grant.not_before_ms != state.not_before_ms {
                state.not_before_monotonic_ms = not_before;
            }
        } else {
            // A repeated acquire is a fresh ownership claim: the server must
            // have expired the previous lease (the local fence already closed) and
            // must not hand back an older generation for this identity.
            if state.generation > grant.generation && !state.released {
                return Err(Error::Replay);
            }
            state.generation = grant.generation;
            state.not_before_monotonic_ms = not_before;
        }
        state.not_before_ms = grant.not_before_ms;
        state.expires_at_ms = grant.expires_at_ms;
        state.deadline_monotonic_ms = deadline;
        // `grant` is now fully validated. The acquire answer is the only source
        // of a token; a renewal leaves the held token exactly as it is, and the
        // differential check above already refused a rotated one.
        if let Some(acquired) = grant.token.take() {
            state.token = acquired;
        }
        state.released = false;
        Ok(state.generation)
    }

    /// The process-monotonic sample in this lease's clock domain.
    #[must_use]
    pub fn monotonic_ms(&self) -> u64 {
        (self.endpoint.clock.monotonic_ms)()
    }

    /// The `Exchange-Publisher-Lease` / `Exchange-Publisher-Boot-Nonce` pair for
    /// a root request, or [`None`] once the lease is released. The token copy is
    /// zeroizing and must not be logged, formatted or URL-encoded.
    #[must_use]
    pub fn lease_headers(&self) -> Option<(Zeroizing<String>, String)> {
        let state = lock(&self.state);
        if state.released || state.token.is_empty() {
            return None;
        }
        Some((state.token.clone(), self.boot_nonce.clone()))
    }

    /// The header pair *and* the generation it belongs to, read under one lock,
    /// so a root request can never pair one generation with another's token.
    fn headers_for(&self, generation: u32) -> Option<(Zeroizing<String>, String)> {
        let state = lock(&self.state);
        if state.released || state.token.is_empty() || state.generation != generation {
            return None;
        }
        Some((state.token.clone(), self.boot_nonce.clone()))
    }

    fn sample(&self) -> Result<(u64, u64)> {
        let wall = (self.endpoint.clock.wall_ms)()?;
        if wall > MAX_SAFE_INTEGER {
            return Err(Error::Invalid);
        }
        let monotonic = (self.endpoint.clock.monotonic_ms)();
        Ok((wall, monotonic))
    }
}

impl LeaseFence for PublisherLease {
    /// The cheap local gate: one mutex, no I/O. Returns `None` once the lease is
    /// released, its local deadline passed, or the server's `notBeforeMs` (plus
    /// uncertainty) has not elapsed yet.
    fn permit(&self, _now_ms: u64) -> Option<LeasePermit> {
        let state = lock(&self.state);
        if state.released {
            return None;
        }
        let (wall, monotonic) = self.sample().ok()?;
        // Server-authoritative wall-clock bound, with uncertainty.
        if wall.saturating_add(self.endpoint.uncertainty_ms) >= state.expires_at_ms {
            return None;
        }
        // A new root must not encrypt before the previous epoch's tail ended.
        if monotonic < state.not_before_monotonic_ms {
            return None;
        }
        // Local monotonic bound: never later than what the authority honours.
        if monotonic.saturating_add(self.endpoint.scheduler_margin_ms)
            >= state.deadline_monotonic_ms
        {
            return None;
        }
        Some(LeasePermit {
            until_monotonic_ms: state.deadline_monotonic_ms,
        })
    }
}

/// Derive the conservative local bounds. The deadline can only be earlier than
/// the server's expiry, the `notBefore` instant only later than the server's.
fn conservatively_bound(
    endpoint: &LeaseEndpoint,
    grant: &LeaseGrant,
    wall_ms: u64,
    monotonic_ms: u64,
) -> Result<(u64, u64)> {
    if grant.expires_at_ms <= wall_ms {
        return Err(Error::Expired);
    }
    let deadline = monotonic_ms
        .checked_add(grant.expires_at_ms - wall_ms)
        .ok_or(Error::Invalid)?
        .saturating_sub(endpoint.uncertainty_ms);
    let not_before = if grant.not_before_ms <= wall_ms {
        monotonic_ms
    } else {
        monotonic_ms
            .checked_add(grant.not_before_ms - wall_ms)
            .ok_or(Error::Invalid)?
            .saturating_add(endpoint.uncertainty_ms)
    };
    Ok((deadline, not_before))
}

/// Validate one acquire/renew answer. Never logs any field.
///
/// The route decides how the token is treated, because the frozen contract is
/// route-specific:
///
/// * an **acquire** answer must carry the canonical `leaseToken`, which is the
///   one and only time the authority hands it out;
/// * a **renew** answer is metadata only. A missing token is the norm: the held
///   token is retained. A token that is present is validated canonically and
///   returned, so the caller can compare it with the held one byte for byte and
///   refuse a divergent (rotated) value rather than adopt it. Nothing here ever
///   installs a token on a renew.
///
/// When the answer echoes an identity (`machineId`, `bootNonce`) it must match
/// the exact opaque string this process sent, byte for byte; there is no
/// normalisation or case folding on either side.
fn parse_lease(
    body: &Value,
    ids: &HttpResourceIds,
    boot_nonce: &str,
    renewing: bool,
) -> Result<LeaseGrant> {
    let generation =
        u32::try_from(safe_number(body, "generation")?).map_err(|_| Error::GenerationOverflow)?;
    let token = match body.get("leaseToken").and_then(Value::as_str) {
        Some(token) => {
            if token.len() != LEASE_TOKEN_CHARS || !is_base64url(token) {
                return Err(Error::Invalid);
            }
            Some(Zeroizing::new(token.to_owned()))
        }
        // The acquire answer is the only place the contract mints a token.
        None if !renewing => return Err(Error::Invalid),
        None => None,
    };
    if let Some(echo) = body.get("bootNonce").and_then(Value::as_str) {
        if echo != boot_nonce {
            return Err(Error::Denied);
        }
    }
    if let Some(echo) = body.get("machineId").and_then(Value::as_str) {
        if echo != ids.machine_id() {
            return Err(Error::Denied);
        }
    }
    let expires_at_ms = safe_number(body, "leaseExpiresAtMs")?;
    let not_before_ms = safe_number(body, "notBeforeMs")?;
    if expires_at_ms <= not_before_ms || not_before_ms == 0 {
        return Err(Error::Invalid);
    }
    Ok(LeaseGrant {
        generation,
        token,
        expires_at_ms,
        not_before_ms,
    })
}

/// One lease wire call. `route` is `acquire`, `renew` or `release`; all are
/// `POST`s on the frozen prefix and none carries a token or nonce in the URL.
fn send_lease(
    endpoint: &LeaseEndpoint,
    route: &str,
    body: Option<Value>,
    expected: u16,
) -> Result<Value> {
    let body = body
        .map(|v| serde_json::to_vec(&v).map_err(|_| Error::Invalid))
        .transpose()?;
    let request = AuthorityHttpRequest {
        method: "POST",
        url: resource_url(&endpoint.base, &endpoint.ids, &["lease", route])?,
        body,
        lease_headers: None,
    };
    endpoint
        .last_status
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let response = endpoint.transport.send(request, &endpoint.credential)?;
    // Status code only (never body or headers), for operator diagnostics.
    endpoint
        .last_status
        .store(response.status, std::sync::atomic::Ordering::Relaxed);
    // A lease answer is no-store secret material and is bounded like any other.
    if response.body.len() > MAX_RESPONSE_BYTES {
        return Err(Error::Capacity);
    }
    if response.status != expected {
        // 409 carries no distinguishing information this adapter may act on: a
        // foreign holder and a lost token both mean "do not seal, do not retry".
        return Err(Error::Denied);
    }
    serde_json::from_slice(&response.body).map_err(|_| Error::Invalid)
}

fn is_base64url(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Accept an origin-only HTTPS base and return it as a parsed [`Url`] so every
/// later request is assembled by the URL API rather than by string formatting.
pub(crate) fn validated_origin(origin: &str) -> Result<Url> {
    let url = Url::parse(origin).map_err(|_| Error::Invalid)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(Error::Denied);
    }
    Ok(url)
}

/// Build `{origin}/v0/organisations/{org_id}/exchange/versions/{version_id}/publisher/...`.
///
/// Each opaque id is pushed as **one path segment**, so the URL API
/// percent-encodes anything structural (`/`, `?`, `#`, `%`, space, non-ASCII)
/// and an id can never add a segment, escape the prefix or inject a query.
fn resource_url(base: &Url, ids: &HttpResourceIds, suffix: &[&str]) -> Result<String> {
    let mut url = base.clone();
    {
        let mut segments = url.path_segments_mut().map_err(|_| Error::Denied)?;
        segments.pop_if_empty();
        segments
            .push("v0")
            .push("organisations")
            .push(ids.org_id())
            .push("exchange")
            .push("versions")
            .push(ids.version_id())
            .push("publisher");
        for segment in suffix {
            segments.push(segment);
        }
    }
    Ok(url.to_string())
}

/// The current-epoch URL. `machineId` is appended as one query pair, so an id
/// containing `&` or `=` cannot introduce a second parameter.
fn current_epoch_url(base: &Url, ids: &HttpResourceIds, generation: u32) -> Result<String> {
    let mut url =
        Url::parse(&resource_url(base, ids, &["current-epoch"])?).map_err(|_| Error::Invalid)?;
    url.query_pairs_mut()
        .append_pair("machineId", ids.machine_id())
        .append_pair("generation", &generation.to_string());
    // `query_pairs_mut` writes application/x-www-form-urlencoded, where a space
    // becomes `+`. An authority that percent-decodes the query without form
    // semantics would then read a different id, so express the query with %XX
    // escapes only. The `+` characters present here are exactly the rewritten
    // spaces: a literal `+` in an id has already been encoded as `%2B`.
    if let Some(query) = url.query() {
        let escaped = query.replace('+', "%20");
        url.set_query(Some(&escaped));
    }
    Ok(url.to_string())
}

/// HTTP authority adapter bound to one org/version/machine, a server-issued
/// publisher generation and an exclusive lease. All registration happens
/// off-clock before roots. A root response cannot silently select another
/// generation, identity or lease.
///
/// The generation is **authoritative and opaque**: the amendment makes every
/// `acquire` (including the first) assign a fresh generation and invalidate the
/// previous lease's roots, so the adapter has no initial guess — `0` means
/// "adopted nothing yet" and a root is never requested until a live lease
/// supplies a generation.
pub struct HttpAuthorityRootProvider {
    /// Origin-only URL; every request is built from it with the URL API.
    base: Url,
    /// The exact opaque ids used in the path, query and body.
    ids: HttpResourceIds,
    /// The explicitly registered raw crypto identity. Never derived from `ids`.
    binding: CryptoBinding,
    /// Highest generation adopted from a lease answer. A high-water mark only:
    /// it is never raised by guessing and never lowered.
    generation: Mutex<Option<u32>>,
    credential: OrganisationCsk,
    transport: Arc<dyn AuthorityHttpTransport>,
    now_ms: Arc<dyn Fn() -> Result<u64> + Send + Sync>,
    lease: Option<Arc<PublisherLease>>,
}
impl HttpAuthorityRootProvider {
    /// Bind a provider to a previously registered generation. The caller must
    /// preserve that server-issued generation across process restarts.
    ///
    /// `ids` are the authority's opaque HTTP identifiers and `binding` is the
    /// raw crypto identity registered for them; passing one without the other,
    /// or a mismatched pair, is refused rather than repaired.
    pub fn new(
        origin: &str,
        ids: HttpResourceIds,
        binding: CryptoBinding,
        generation: Option<u32>,
        credential: OrganisationCsk,
    ) -> Result<Self> {
        Self::with_transport(
            origin,
            ids,
            binding,
            generation,
            credential,
            Arc::new(VerifiedHttpsTransport::new()?),
        )
    }

    /// Inject a transport for tests or for an audited runtime transport. This
    /// form has no lease; call [`with_lease`](Self::with_lease) before any
    /// protected seal, or the publisher refuses to seal.
    ///
    /// `generation` is accepted for source compatibility with the pre-amendment
    /// API and is **ignored**: the amendment makes every `acquire` assign a
    /// generation, so the adapter starts with none and takes it from the live
    /// lease (see [`adopt_generation`](Self::adopt_generation)).
    #[allow(unused_variables)]
    pub fn with_transport(
        origin: &str,
        ids: HttpResourceIds,
        binding: CryptoBinding,
        generation: Option<u32>,
        credential: OrganisationCsk,
        transport: Arc<dyn AuthorityHttpTransport>,
    ) -> Result<Self> {
        let base = validated_origin(origin)?;
        Ok(Self {
            base,
            ids,
            binding,
            generation: Mutex::new(None),
            credential,
            transport,
            now_ms: Arc::new(|| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| Error::Invalid)?;
                u64::try_from(now.as_millis()).map_err(|_| Error::Invalid)
            }),
            lease: None,
        })
    }

    /// Bind the exclusive lease this provider requires before it will fetch or
    /// serve a root. The same `Arc` must be attached to the
    /// [`AuthorityRootGuard`](crate::AuthorityRootGuard) so the publisher
    /// re-checks it on every seal.
    #[must_use]
    pub fn with_lease(mut self, lease: Arc<PublisherLease>) -> Self {
        self.lease = Some(lease);
        self
    }

    /// The bound lease, if any.
    #[must_use]
    pub fn lease(&self) -> Option<&Arc<PublisherLease>> {
        self.lease.as_ref()
    }

    /// The exact opaque ids this provider addresses.
    #[must_use]
    pub fn ids(&self) -> &HttpResourceIds {
        &self.ids
    }

    /// The explicitly registered crypto binding used for the epoch-root context.
    #[must_use]
    pub fn binding(&self) -> CryptoBinding {
        self.binding
    }

    /// Inject a deterministic clock for transport tests. Protected deployments
    /// must use the default system clock and not override it.
    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> Result<u64> + Send + Sync>) -> Self {
        self.now_ms = clock;
        self
    }

    /// Highest generation adopted from the lease, if any. Persist it with the
    /// durable counter journal; it is assigned by `acquire`/`renew`, never by
    /// the caller.
    pub fn generation(&self) -> Option<u32> {
        *lock(&self.generation)
    }

    /// Adopt the generation the lease assigned. A lower value than the one
    /// already adopted is refused, so a stale or replayed lease answer cannot
    /// rewind the identity. Every acquire increments the generation, so a later
    /// acquire that answers with a lower value is an authority inconsistency.
    pub fn adopt_generation(&self, next: u32) -> Result<()> {
        let mut generation = lock(&self.generation);
        if generation.is_some_and(|previous| next < previous) {
            return Err(Error::Replay);
        }
        *generation = Some(next);
        Ok(())
    }

    /// Register this publisher machine against the opaque resource ids and
    /// obtain the server-issued generation.
    ///
    /// This is the pre-amendment registration route (the 0770 contract), kept
    /// alongside the lease. It must not be called on the output clock, and it
    /// does not by itself authorise a root: the protected root path still needs
    /// a live lease, whose `acquire`/`renew` generation is authoritative. The
    /// request body carries the exact opaque machine id.
    pub fn register(&self) -> Result<u32> {
        let url = resource_url(&self.base, &self.ids, &[])?;
        let (status, reply) = self.send_raw(
            "POST",
            url,
            Some(json!({"machineId": self.ids.machine_id()})),
            None,
        )?;
        if status != 201 {
            return Err(Error::Denied);
        }
        let next = self.parse_identity(&reply)?;
        self.adopt_generation(next)?;
        Ok(next)
    }

    /// Rotate from the currently registered server generation.
    ///
    /// Only the currently adopted generation may rotate; the answer must be
    /// strictly greater. The response ids are checked byte-for-byte.
    pub fn rotate(&self) -> Result<u32> {
        let old = self.generation().ok_or(Error::Denied)?;
        let url = resource_url(&self.base, &self.ids, &["rotation"])?;
        let (status, reply) = self.send_raw(
            "POST",
            url,
            Some(json!({
                "machineId": self.ids.machine_id(),
                "expectedGeneration": old,
            })),
            None,
        )?;
        if status != 201 {
            return Err(Error::Denied);
        }
        let next = self.parse_identity(&reply)?;
        if next <= old {
            return Err(Error::Replay);
        }
        self.adopt_generation(next)?;
        Ok(next)
    }

    /// Check the authenticated response's echoed ids against the exact opaque
    /// strings this process sent, then read the generation.
    ///
    /// Equality is byte-for-byte: a response that normalises, strips a prefix or
    /// otherwise rewrites an id is a different resource and is refused.
    fn parse_identity(&self, body: &Value) -> Result<u32> {
        if body.get("versionId").and_then(Value::as_str) != Some(self.ids.version_id())
            || body.get("publisherMachineId").and_then(Value::as_str) != Some(self.ids.machine_id())
        {
            return Err(Error::Denied);
        }
        let value = safe_number(body, "generation")?;
        u32::try_from(value).map_err(|_| Error::GenerationOverflow)
    }

    /// One bounded exchange, returning the status with the parsed body so the
    /// caller can distinguish an authoritative rejection (409 no lease, 404
    /// stale lease) from a generic failure.
    fn send_raw(
        &self,
        method: &'static str,
        url: String,
        body: Option<Value>,
        lease_headers: Option<LeaseHeaders>,
    ) -> Result<(u16, Value)> {
        let body = body
            .map(|v| serde_json::to_vec(&v).map_err(|_| Error::Invalid))
            .transpose()?;
        let response = self.transport.send(
            AuthorityHttpRequest {
                method,
                url,
                body,
                lease_headers,
            },
            &self.credential,
        )?;
        if response.body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Capacity);
        }
        let reply = serde_json::from_slice(&response.body).map_err(|_| Error::Invalid)?;
        Ok((response.status, reply))
    }
}
impl AuthorityRootProvider for HttpAuthorityRootProvider {
    fn current_epoch_root(&self, request: &EpochRootRequest) -> Result<AuthorityEpochRoot> {
        // The request must carry the explicitly registered crypto binding. It is
        // never derived from the opaque ids, and a request for another binding is
        // refused before anything is sent.
        if request.org != self.binding.org
            || request.version != self.binding.version
            || request.publisher_machine != self.binding.publisher_machine
            || request.now_ms > MAX_SAFE_INTEGER
            || request.epoch != request.now_ms / EPOCH_MS
        {
            return Err(Error::Denied);
        }
        let before = (self.now_ms)()?;
        if before < request.now_ms
            || before > MAX_SAFE_INTEGER
            || before / EPOCH_MS != request.epoch
        {
            return Err(Error::Expired);
        }
        // The lease gate comes first, before any wire call and before the local
        // generation is trusted. A cached root is served only while the lease is
        // valid and the server's notBefore instant has elapsed.
        let lease = self.lease.as_ref().ok_or(Error::Denied)?;
        // The lease must have been registered for exactly these opaque ids and
        // this crypto binding. A mismatched or absent pairing fails closed; no
        // mapping is invented from one form to the other.
        if lease.ids() != &self.ids || lease.binding() != self.binding {
            return Err(Error::Denied);
        }
        // The fence, not the fetch, decides whether a root may exist at all.
        let permit = LeaseFence::permit(lease.as_ref(), before).ok_or(Error::Expired)?;
        // The live lease is the only source of the generation: no initial guess,
        // and a lease the server no longer honours cannot request a root.
        let expected_generation = lease.generation().ok_or(Error::Denied)?;
        // Retain the authoritative lease high-water even if the HTTP fetch fails.
        self.adopt_generation(expected_generation)?;
        let (token, boot_nonce) = lease
            .headers_for(expected_generation)
            .ok_or(Error::Denied)?;
        let headers = LeaseHeaders { token, boot_nonce };
        let url = current_epoch_url(&self.base, &self.ids, expected_generation)?;
        let reply = match self.send_raw("GET", url, None, Some(headers))? {
            (200, reply) => reply,
            (409, _) => {
                // `lease_required`: this process has no valid lease for the
                // route. Close the local fence rather than retrying.
                lease.revoke_with_status(409);
                return Err(Error::Denied);
            }
            (404, _) => {
                // Stale lease / superseded generation: authoritative, so the
                // lease is dead here too. A new process (and boot nonce) is
                // needed; never re-acquire implicitly in this one.
                lease.revoke_with_status(404);
                return Err(Error::Denied);
            }
            _ => return Err(Error::Denied),
        };
        // A fence that closed while the answer was in flight (release,
        // revocation, withdrawal, lapsed deadline) is not timing: it is checked
        // first, before any epoch check, and answered as `Denied`, so no caller
        // can mistake it for an answer overtaken by the boundary and retry it.
        let answered = (self.now_ms)()?;
        LeaseFence::permit(lease.as_ref(), answered).ok_or(Error::Denied)?;
        let generation = self.parse_identity(&reply)?;
        if generation != expected_generation {
            return Err(Error::Replay);
        }
        let observed = (self.now_ms)()?;
        if observed < before || observed > MAX_SAFE_INTEGER || observed / EPOCH_MS != request.epoch
        {
            return Err(Error::Expired);
        }
        let epoch = safe_number(&reply, "epoch")?;
        let start = safe_number(&reply, "epochStartMs")?;
        let end = safe_number(&reply, "epochEndMs")?;
        if epoch != request.epoch
            || start != epoch.checked_mul(EPOCH_MS).ok_or(Error::Invalid)?
            || end != start.checked_add(EPOCH_MS).ok_or(Error::Invalid)?
            || observed < start
            || observed >= end
        {
            return Err(Error::Expired);
        }
        let encoded = reply
            .get("rootKeyBase64")
            .and_then(Value::as_str)
            .ok_or(Error::Invalid)?;
        if encoded.len() != 44 || !encoded.ends_with('=') {
            return Err(Error::Invalid);
        }
        let decoded = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Error::Invalid)?,
        );
        let root: [u8; 32] = decoded.as_slice().try_into().map_err(|_| Error::Invalid)?;
        // A slow decode cannot turn this into a late seal. A fresh caller must
        // request the new epoch instead; never prefetch one here. The lease is
        // re-checked after the decode so a root fetched while the lease was
        // still alive is not handed out once it has closed.
        let after = (self.now_ms)()?;
        if after < observed || after >= end || after / EPOCH_MS != request.epoch {
            return Err(Error::Expired);
        }
        LeaseFence::permit(lease.as_ref(), after).ok_or(Error::Denied)?;
        // The root's local expiry is never later than the lease the authority
        // actually honours, and never outside this epoch: the permit deadline
        // translated back into wall clock, clamped to the epoch end.
        let expiry = observed
            .saturating_add(
                permit
                    .until_monotonic_ms
                    .saturating_sub(lease.monotonic_ms()),
            )
            .min(end);
        if expiry <= observed {
            return Err(Error::Expired);
        }
        // The crypto context is built from the explicitly registered raw binding
        // (checked above and against the opaque response ids), never from the
        // opaque strings. The response's echoed ids were checked byte-for-byte
        // in `parse_identity` before this point; `from_authority` independently
        // checks exact identities and monotonicity again.
        Ok(AuthorityEpochRoot::new(
            self.binding.org,
            self.binding.version,
            request.feed,
            self.binding.publisher_machine,
            request.publisher,
            u64::from(generation),
            epoch,
            start,
            expiry,
            root,
        ))
    }
}
fn safe_number(value: &Value, field: &str) -> Result<u64> {
    let number = value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or(Error::Invalid)?;
    if number > MAX_SAFE_INTEGER {
        return Err(Error::Invalid);
    }
    Ok(number)
}

#[cfg(test)]
mod grant_framing_tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn raw_length_and_unambiguous_framing_are_required() {
        let mut h = HeaderMap::new();
        for status in [401, 404, 503] {
            assert_eq!(grant_content_length(status, &h), Err(Error::Denied));
        }
        assert_eq!(grant_content_length(200, &h), Err(Error::Invalid));
        h.insert("content-length", HeaderValue::from_static("264"));
        assert_eq!(grant_content_length(200, &h), Ok(264));
        for name in ["transfer-encoding", "content-encoding"] {
            h.insert(name, HeaderValue::from_static("chunked"));
            assert_eq!(grant_content_length(200, &h), Err(Error::Invalid));
            h.remove(name);
        }
        h.append("content-length", HeaderValue::from_static("264"));
        assert_eq!(grant_content_length(200, &h), Err(Error::Invalid));
        for value in ["263", "265", "264, 264", "0264", "bad"] {
            h.insert("content-length", HeaderValue::from_static(value));
            assert_eq!(grant_content_length(200, &h), Err(Error::Invalid));
        }
    }
}

#[cfg(test)]
mod status_log_tests {
    use super::*;
    #[test]
    fn route_template_redacts_ids_query_and_host() {
        assert_eq!(
            route_template("https://[::1]:8443/v0/organisations/org_secret/exchange/bookings/bk:org:ref/grants"),
            "/v0/organisations/{org}/exchange/bookings/{booking}/grants"
        );
        assert_eq!(
            route_template("https://h/v0/organisations/o/exchange/versions/v/publisher/current-epoch?machineId=mac&generation=1"),
            "/v0/organisations/{org}/exchange/versions/{version}/publisher/current-epoch"
        );
    }
}
