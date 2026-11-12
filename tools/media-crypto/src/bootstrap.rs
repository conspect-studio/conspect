//! Private two-phase harness control plane. Never run on an output-clock thread.
//! Bootstrap grants are deliberately discarded: they are not identity evidence.
use crate::authority_http::{
    AuthorityHttpRequest, AuthorityHttpTransport, CryptoBinding, HttpAuthorityRootProvider,
    HttpResourceIds, LeaseClock, OrganisationCsk, PublisherLease,
};
use crate::{AuthorityRootProvider, EpochRootRequest, LeaseFence};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::sync::Arc;
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub type Result<T> = std::result::Result<T, &'static str>;
const LIMIT: u64 = 65536;
const EPOCH_MS: u64 = 5000;
const CLOCK_UNCERTAINTY_MS: u64 = 1000;
const NETWORK_BUDGET_MS: u64 = 500;
const SETTLE_MARGIN_MS: u64 = 250;

/// Only control-plane admission uses this clock; the canonical lease has its own clock.
pub trait EpochAdmissionClock: Send + Sync {
    fn wall_ms(&self) -> Result<u64>;
    /// Implementations sleep against a monotonic deadline, never polling the wall clock.
    fn sleep(&self, duration: Duration);
}
struct SystemAdmissionClock;
impl EpochAdmissionClock for SystemAdmissionClock {
    fn wall_ms(&self) -> Result<u64> {
        now()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
fn epoch_end(wall: u64) -> Result<u64> {
    (wall / EPOCH_MS)
        .checked_add(1)
        .and_then(|e| e.checked_mul(EPOCH_MS))
        .ok_or("clock invalid")
}
/// One monotonic wait derived from one wall sample; always resample after waking.
fn boundary_wait(clock: &dyn EpochAdmissionClock, wall: u64) -> Result<u64> {
    let duration = Duration::from_millis(
        epoch_end(wall)?
            .saturating_sub(wall)
            .saturating_add(SETTLE_MARGIN_MS),
    );
    let deadline = Instant::now()
        .checked_add(duration)
        .ok_or("clock invalid")?;
    // Round upward: a test clock with whole-millisecond resolution must not
    // truncate the 250 ms settle margin by one millisecond.
    clock.sleep(deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    clock.wall_ms()
}
fn admitted_wall(clock: &dyn EpochAdmissionClock) -> Result<u64> {
    let wall = clock.wall_ms()?;
    if epoch_end(wall)?.saturating_sub(wall) < CLOCK_UNCERTAINTY_MS + NETWORK_BUDGET_MS {
        boundary_wait(clock, wall)
    } else {
        Ok(wall)
    }
}

/// Open first, then fstat. No symlinks, devices, hardlinks or permissive files.
pub fn private_json(path: &Path) -> Result<Value> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory =
        crate::private_state::PrivateDir::open(parent).map_err(|_| "private parent rejected")?;
    let bytes = Zeroizing::new(
        directory
            .read(path, LIMIT)
            .map_err(|_| "private file boundary rejected")?
            .ok_or("private file unavailable")?,
    );
    serde_json::from_slice(&bytes).map_err(|_| "private JSON invalid")
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.bytes().any(|b| b.is_ascii_control()))
        .ok_or("required text missing")
}
fn id(v: &Value, key: &str) -> Result<String> {
    let s = text(v, key)?;
    if s.len() > 200
        || s == "."
        || s == ".."
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_:-".contains(&b))
    {
        return Err("opaque id rejected");
    }
    Ok(s.into())
}
fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| "clock invalid")
}
fn raw_identity(v: &Value, key: &str) -> Result<[u8; 16]> {
    let s = text(v, key)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| "publisher mapping absent or invalid")?;
    if URL_SAFE_NO_PAD.encode(&bytes) != s {
        return Err("publisher mapping noncanonical");
    }
    bytes.try_into().map_err(|_| "publisher mapping width")
}
#[derive(Clone)]
struct Actor {
    org: String,
    machine: String,
    csk: OrganisationCsk,
}
impl Actor {
    fn parse(v: &Value) -> Result<Self> {
        Ok(Self {
            org: id(v, "orgId")?,
            machine: id(v, "machineId")?,
            csk: OrganisationCsk::new(text(v, "token")?.into())
                .map_err(|_| "credential rejected")?,
        })
    }
}
/// Identity-only transport gate. Root syntax, time, key and lease validation stay canonical.
struct PinnedRootTransport {
    inner: Arc<dyn AuthorityHttpTransport>,
    binding: CryptoBinding,
    feed: [u8; 16],
}
impl AuthorityHttpTransport for PinnedRootTransport {
    fn send(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
    ) -> crate::Result<crate::authority_http::AuthorityHttpResponse> {
        let response = self.inner.send(request, credential)?;
        if response.status == 200 {
            let body: Value =
                serde_json::from_slice(&response.body).map_err(|_| crate::Error::Invalid)?;
            for (field, expected) in [
                ("contributorOrgCryptoId", self.binding.org),
                ("versionCryptoId", self.binding.version),
                ("publisherMachineCryptoId", self.binding.publisher_machine),
                ("feedCryptoId", self.feed),
            ] {
                if raw_identity(&body, field).ok() != Some(expected) {
                    return Err(crate::Error::Denied);
                }
            }
        }
        Ok(response)
    }
}
/// Public, non-secret fixture controls. Wallet metadata stays decimal text (never f64).
#[derive(Clone)]
pub struct FixtureProfile {
    funded: bool,
    duration: u64,
    extra: u64,
    due: u64,
    detail: String,
}
impl FixtureProfile {
    pub fn parse(v: &Value) -> Result<Self> {
        let name = if v.is_null() {
            "standard"
        } else {
            v.get("name").map_or(Ok("standard"), |n| {
                n.as_str().ok_or("profile name rejected")
            })?
        };
        if name == "standard" {
            return Ok(Self {
                funded: false,
                duration: 60000,
                extra: 20000,
                due: 15000,
                detail: String::new(),
            });
        }
        if name != "funded-expiry" {
            return Err("profile name rejected");
        }
        let number = |key, default| -> Result<u64> {
            v.get(key).map_or(Ok(default), |n| {
                n.as_u64()
                    .filter(|n| *n > 0 && *n <= 9007199254740991)
                    .ok_or("profile integer rejected")
            })
        };
        let duration = number("bookingDurationMs", 10000)?;
        let extra = number("extraDurationMs", 5000)?;
        let due = number("extensionDueAfterBookingMs", 3000)?;
        number("rateMicroPerMinute", 600000)?;
        if due >= duration
            || duration > 60000
            || extra > 60000
            || number("expectedExtensionStatus", 409)? != 409
        {
            return Err("profile window rejected");
        }
        if v.get("expectedExtensionCause")
            .is_some_and(|n| n != "insufficient_credit")
        {
            return Err("profile cause rejected");
        }
        let detail = v
            .get("expectedExtensionDetailContains")
            .map_or(Ok("prepaid exchange balance cannot cover"), |n| {
                n.as_str()
                    .filter(|s| {
                        !s.is_empty() && s.contains("prepaid exchange balance cannot cover")
                    })
                    .ok_or("profile detail rejected")
            })?
            .to_owned();
        for key in ["takerAAvailableMicro", "takerBAvailableMicro"] {
            if let Some(n) = v.get(key) {
                let s = n.as_str().ok_or("profile funding must be decimal text")?;
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    return Err("profile funding rejected");
                }
            }
        }
        Ok(Self {
            funded: true,
            duration,
            extra,
            due,
            detail,
        })
    }
    pub fn funded_expiry(&self) -> bool {
        self.funded
    }
    pub fn booking_duration_ms(&self, i: usize) -> u64 {
        if i == 0 {
            self.duration
        } else {
            60000
        }
    }
}

/// No Debug: this owns bearer credentials and the in-process canonical lease.
#[derive(Clone)]
pub struct Bootstrap {
    origin: String,
    version: String,
    contributor: Actor,
    takers: [Actor; 2],
    nonce: String,
    lease: Option<Arc<PublisherLease>>,
    provider: Option<Arc<HttpAuthorityRootProvider>>,
    binding: CryptoBinding,
    feed: [u8; 16],
    publisher_instance: [u8; 16],
    deadline: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,

    profile: FixtureProfile,
    booked_at: Option<Instant>,
    /// Wall ms at which the expected credit refusal was observed (funded-expiry only).
    expected_extension: Arc<std::sync::Mutex<Option<u64>>>,
    bookings: Option<[String; 2]>,
    booking_expiries: Arc<std::sync::Mutex<Option<[u64; 2]>>>,
    admission_clock: Arc<dyn EpochAdmissionClock>,
    expected_validated: Arc<std::sync::atomic::AtomicBool>,
}
impl Bootstrap {
    /// Validate supported frozen fields only. Unknown delta fields confer no trust.
    pub fn parse(v: &Value) -> Result<Self> {
        if v["kind"] != "exchange-connected-proof-harness"
            || v["basePath"] != "/v0"
            || v["scenario"]["publisherLease"]["bootstrapOnly"] != true
            || v["scenario"]["publisherLease"]["adoptable"] != false
            || ["generation", "bootNonce", "leaseToken", "leaseExpiresAtMs"]
                .iter()
                .any(|k| v["scenario"]["publisherLease"].get(k).is_some())
        {
            return Err("bootstrap schema rejected");
        }
        let origin = text(v, "baseUrl")?.trim_end_matches('/').to_string();
        let url = reqwest::Url::parse(&origin).map_err(|_| "origin rejected")?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err("origin rejected");
        }
        let mut nonce = [0; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| "random unavailable")?;
        Ok(Self {
            origin,
            version: id(&v["scenario"], "versionId")?,
            contributor: Actor::parse(&v["credentials"]["contributor"])?,
            takers: [
                Actor::parse(&v["credentials"]["takerA"])?,
                Actor::parse(&v["credentials"]["takerB"])?,
            ],
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            lease: None,
            provider: None,
            binding: CryptoBinding {
                org: raw_identity(
                    &v["scenario"]["expectedIdentities"]["shared"],
                    "contributorOrg",
                )?,
                version: raw_identity(&v["scenario"]["expectedIdentities"]["shared"], "version")?,
                publisher_machine: raw_identity(
                    &v["scenario"]["expectedIdentities"]["shared"],
                    "publisherMachine",
                )?,
            },
            feed: raw_identity(&v["scenario"]["expectedIdentities"]["shared"], "feed")?,
            publisher_instance: raw_identity(
                &v["scenario"]["expectedIdentities"]["shared"],
                "publisherInstance",
            )?,
            deadline: Default::default(),

            profile: FixtureProfile::parse(&v["scenario"]["fixtureProfile"])?,
            booked_at: None,
            expected_extension: Default::default(),
            bookings: None,
            booking_expiries: Default::default(),
            admission_clock: Arc::new(SystemAdmissionClock),
            expected_validated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
    /// Deterministic clock for bounded admission tests; not the lease authority clock.
    pub fn with_admission_clock(mut self, clock: Arc<dyn EpochAdmissionClock>) -> Self {
        self.admission_clock = clock;
        self
    }
    fn booking_live(&self, i: usize, wall: u64) -> Result<()> {
        if self
            .booking_expiries
            .lock()
            .map_err(|_| "booking lock")?
            .as_ref()
            .and_then(|e| e.get(i))
            .is_some_and(|expiry| *expiry > wall)
        {
            Ok(())
        } else {
            Err("booking expired")
        }
    }
    fn update_booking_expiry(&self, i: usize, row: &Value) -> Result<()> {
        let expiry = row
            .get("expires_at")
            .and_then(Value::as_u64)
            .ok_or("booking expiry missing")?;
        if expiry > 9_007_199_254_740_991 || expiry <= now()? {
            return Err("booking expiry rejected");
        }
        let mut guard = self.booking_expiries.lock().map_err(|_| "booking lock")?;
        let current = guard
            .as_mut()
            .and_then(|e| e.get_mut(i))
            .ok_or("no booking")?;
        if expiry < *current {
            return Err("booking expiry rejected");
        }
        *current = expiry;
        Ok(())
    }
    fn grant_request(
        &self,
        t: &dyn AuthorityHttpTransport,
        i: usize,
        b: &str,
        wall: u64,
    ) -> Result<crate::authority_http::GrantHttpResponse> {
        if !self.lease_valid() {
            return Err("lease expired");
        }
        self.booking_live(i, wall)?;
        let requested = wall
            .saturating_add(4000)
            .min(epoch_end(wall)?.saturating_sub(1));
        if requested <= wall.saturating_add(CLOCK_UNCERTAINTY_MS) {
            return Err("grant near boundary");
        }
        t.send_grant(
            AuthorityHttpRequest {
                method: "POST",
                url: self.booking_action(i, b, "grants")?,
                body: Some(
                    serde_json::to_vec(
                        &json!({"machineId":self.takers[i].machine,"requestedExpiryMs":requested}),
                    )
                    .map_err(|_| "grant JSON")?,
                ),
                lease_headers: None,
            },
            &self.takers[i].csk,
        )
        .map_err(|_| "grant transport")
    }
    /// Exactly one 404 retry on this route, for the same booking and no other status.
    fn bounded_grant(
        &self,
        t: &dyn AuthorityHttpTransport,
        i: usize,
        b: &str,
    ) -> Result<crate::authority_http::GrantHttpResponse> {
        let wall = admitted_wall(self.admission_clock.as_ref())?;
        let first = self.grant_request(t, i, b, wall)?;
        if first.status != 404 {
            return Ok(first);
        }
        // The first denied body is secret-bearing by contract and drops before retry.
        drop(first);
        if !self.lease_valid() {
            return Err("lease expired");
        }
        let wall = boundary_wait(
            self.admission_clock.as_ref(),
            self.admission_clock.wall_ms()?,
        )?;
        self.grant_request(t, i, b, wall)
    }
    fn book_path(&self, taker: usize, tail: &str) -> String {
        format!(
            "{}/v0/organisations/{}/exchange/bookings{}",
            self.origin, self.takers[taker].org, tail
        )
    }
    fn booking_action(&self, taker: usize, booking: &str, action: &str) -> Result<String> {
        let mut url =
            reqwest::Url::parse(&self.book_path(taker, "")).map_err(|_| "booking URL rejected")?;
        url.path_segments_mut()
            .map_err(|_| "booking URL rejected")?
            .push(booking)
            .push(action);
        Ok(url.into())
    }
    fn call(
        t: &dyn AuthorityHttpTransport,
        actor: &Actor,
        url: String,
        body: Option<Value>,
        status: u16,
    ) -> Result<Value> {
        let r = t
            .send(
                AuthorityHttpRequest {
                    method: "POST",
                    url,
                    body: body.map(|v| serde_json::to_vec(&v).expect("JSON values serialize")),
                    lease_headers: None,
                },
                &actor.csk,
            )
            .map_err(|_| "HTTP transport rejected")?;
        if r.status != status {
            return Err("HTTP status rejected");
        }
        let bytes = Zeroizing::new(r.body);
        serde_json::from_slice(&bytes).map_err(|_| "HTTP JSON rejected")
    }
    /// The canonical owner is the only actor allowed to acquire, renew or release.
    pub fn acquire(&mut self, t: Arc<dyn AuthorityHttpTransport>) -> Result<()> {
        if self.lease.is_some() {
            return Err("lease already acquired");
        }
        let ids = HttpResourceIds::new(
            &self.contributor.org,
            &self.version,
            &self.contributor.machine,
        )
        .map_err(|_| "publisher ids")?;
        let lease = Arc::new(
            PublisherLease::with_transport(
                &self.origin,
                ids.clone(),
                self.binding,
                self.contributor.csk.clone(),
                t.clone(),
                LeaseClock::system(),
            )
            .map_err(|_| "lease configuration")?,
        );
        let pinned = Arc::new(PinnedRootTransport {
            inner: t,
            binding: self.binding,
            feed: self.feed,
        });
        let provider = Arc::new(
            HttpAuthorityRootProvider::with_transport(
                &self.origin,
                ids,
                self.binding,
                None,
                self.contributor.csk.clone(),
                pinned,
            )
            .map_err(|_| "provider configuration")?
            .with_lease(lease.clone()),
        );
        // Store before I/O so even an unsuccessful acquisition cannot be retried.
        self.lease = Some(lease.clone());
        self.provider = Some(provider);
        lease.acquire().map_err(|_| "lease acquire")?;
        self.update_deadline()
    }
    fn update_deadline(&self) -> Result<()> {
        let lease = self.lease.as_ref().ok_or("no lease")?;
        let permit = lease.permit(now()?);
        *self.deadline.lock().map_err(|_| "lease lock")? = permit.map(|p| {
            Instant::now()
                + Duration::from_millis(p.until_monotonic_ms.saturating_sub(lease.monotonic_ms()))
        });
        Ok(())
    }
    pub fn renew(&mut self, _t: &dyn AuthorityHttpTransport) -> Result<()> {
        let result = self
            .lease
            .as_ref()
            .ok_or("no lease")?
            .renew()
            .map_err(|_| "lease renewal");
        self.update_deadline()?;
        result.map(|_| ())
    }
    pub fn publisher_lease(&self) -> Result<Arc<PublisherLease>> {
        self.lease.clone().ok_or("no lease")
    }
    pub fn root_provider(&self) -> Result<Arc<HttpAuthorityRootProvider>> {
        self.provider.clone().ok_or("no provider")
    }
    pub fn publisher_context(&self) -> (CryptoBinding, [u8; 16], [u8; 16]) {
        (self.binding, self.feed, self.publisher_instance)
    }
    pub fn book(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        if self.bookings.is_some() {
            return Err("already booked");
        }
        let mut bookings = Vec::new();
        let mut expiries = Vec::new();
        for i in 0..2 {
            let sent = Instant::now();
            let r = Self::call(
                t,
                &self.takers[i],
                self.book_path(i, ""),
                Some(
                    json!({"versionId":self.version,"machineId":self.takers[i].machine,"durationMs":self.profile.booking_duration_ms(i),"reference":format!("engine-{}-{i}",self.nonce)}),
                ),
                201,
            )?;
            let booking = id(&r, "id")?;
            if booking != format!("bk:{}:engine-{}-{i}", self.takers[i].org, self.nonce) {
                return Err("booking reference binding rejected");
            }
            let expiry = r
                .get("expires_at")
                .and_then(Value::as_u64)
                .ok_or("booking expiry missing")?;
            let created = r
                .get("created_at")
                .and_then(Value::as_u64)
                .ok_or("booking creation missing")?;
            if expiry > 9_007_199_254_740_991
                || created.checked_add(self.profile.booking_duration_ms(i)) != Some(expiry)
                || expiry <= now()?
            {
                return Err("booking expiry rejected");
            }
            if i == 0 {
                self.booked_at = Some(sent);
            }
            expiries.push(expiry);
            bookings.push(booking);
        }
        if bookings[0] == bookings[1] {
            return Err("aliased bookings");
        }
        self.bookings = Some(bookings.try_into().map_err(|_| "booking count")?);
        *self.booking_expiries.lock().map_err(|_| "booking lock")? =
            Some(expiries.try_into().map_err(|_| "booking count")?);
        Ok(())
    }
    /// Mapping mint only: discard all grant bytes without using their identities.
    pub fn mint_mapping(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        let end = Instant::now() + Duration::from_secs(8);
        while !self.lease_valid() {
            if Instant::now() >= end {
                return Err("takeover wait exceeded");
            }
            self.renew(t)?;
            std::thread::sleep(Duration::from_millis(100));
        }
        self.root(t)?;
        for i in 0..2 {
            self.renew(t)?;
            let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
            // A transport failure (no answer inside the transport timeout) is a
            // late or lost answer, not a refusal: keep it distinguishable so the
            // runtime can retry it on its own bounded budget. Only an answered
            // non-200 or a malformed body is "mapping grant rejected".
            let r = self
                .bounded_grant(t, i, b)
                .map_err(|_| "mapping grant transport")?;
            let bytes = Zeroizing::new(r.body);
            if r.status != 200 || bytes.len() != 264 {
                return Err("mapping grant rejected");
            }
        }
        Ok(())
    }
    pub fn write_marker(&self, path: &Path) -> Result<()> {
        let b = self.bookings.as_ref().ok_or("no bookings")?;
        let bytes = serde_json::to_vec(&json!({"bookingA":b[0],"bookingB":b[1]}))
            .map_err(|_| "marker JSON")?;
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| "marker creation rejected")?;
        f.write_all(&bytes)
            .and_then(|_| f.sync_all())
            .map_err(|_| "marker write failed")
    }
    /// Bounded coordinator handoff. Timer sleeps only on this control worker.
    /// Unknown/missing expected-fragment schema remains fail-closed: no media.
    pub fn wait_expected(&mut self, t: &dyn AuthorityHttpTransport, path: &Path) -> Result<Value> {
        let end = Instant::now() + Duration::from_secs(120);
        let mut renew = Instant::now() + Duration::from_secs(1);
        let mut extend = Instant::now() + Duration::from_secs(20);
        let mut seq = 0;
        loop {
            if Instant::now() >= end {
                return Err("expected fragment timeout");
            }
            if Instant::now() >= renew {
                self.renew(t)?;
                renew = Instant::now() + Duration::from_secs(1);
            }
            if self.profile.funded && Instant::now() >= self.extension_due(0)? {
                self.extend_booking(t, 0, 1)?;
            }
            if Instant::now() >= extend {
                seq += 1;
                for i in 0..2 {
                    if i == 0 && self.profile.funded {
                        continue;
                    }
                    let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
                    let row = Self::call(
                        t,
                        &self.takers[i],
                        self.booking_action(i, b, "extend")?,
                        Some(
                            json!({"reference":format!("extend-{}-{seq}-{i}",self.nonce),"extraDurationMs":20000}),
                        ),
                        201,
                    )?;
                    self.update_booking_expiry(i, &row)?;
                }
                extend = Instant::now() + Duration::from_secs(20);
            }
            match std::fs::symlink_metadata(path) {
                Ok(_) => return private_json(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err("expected fragment inaccessible"),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    pub fn funded_expiry(&self) -> bool {
        self.profile.funded
    }
    /// Booking A's paid-window end: the authority's `expires_at` (Unix ms) from the 201 row.
    pub fn booking_expiry(&self) -> Result<u64> {
        self.booking_expiries
            .lock()
            .map_err(|_| "booking lock")?
            .map(|e| e[0])
            .ok_or("booking expiry absent")
    }
    pub fn extension_observed(&self) -> bool {
        self.expected_extension.lock().is_ok_and(|v| v.is_some())
    }
    /// Measured wall ms of the early credit refusal, for the run report.
    pub fn extension_refused_at(&self) -> Option<u64> {
        self.expected_extension.lock().ok().and_then(|v| *v)
    }
    pub fn extension_due(&self, i: usize) -> Result<Instant> {
        if i == 0 && self.profile.funded {
            Ok(self.booked_at.ok_or("not booked")? + Duration::from_millis(self.profile.due))
        } else {
            Ok(Instant::now() + Duration::from_secs(15))
        }
    }
    /// Extend only active bookings. References are unique across control rounds.
    pub fn extend_booking(&self, t: &dyn AuthorityHttpTransport, i: usize, seq: u64) -> Result<()> {
        let b = self
            .bookings
            .as_ref()
            .and_then(|b| b.get(i))
            .ok_or("no booking")?;
        if i == 0 && self.profile.funded {
            let mut observed = self
                .expected_extension
                .lock()
                .map_err(|_| "extension lock")?;
            if observed.is_some() {
                return Ok(());
            }
            if now()? >= self.booking_expiry()? {
                return Err("credit extension missed window");
            }
            let reply = Self::call(
                t,
                &self.takers[i],
                self.booking_action(i, b, "extend")?,
                Some(
                    json!({"reference":format!("native-extend-{}-{seq}-{i}",self.nonce),"extraDurationMs":self.profile.extra}),
                ),
                409,
            )?;
            if reply["status"] != 409
                || reply["type"].as_str().is_none()
                || reply["title"].as_str().is_none()
                || !reply["detail"]
                    .as_str()
                    .is_some_and(|s| s.contains(&self.profile.detail))
                || now()? >= self.booking_expiry()?
            {
                return Err("credit extension problem rejected");
            }
            *observed = Some(now()?);
            return Ok(());
        }
        let row = Self::call(
            t,
            &self.takers[i],
            self.booking_action(i, b, "extend")?,
            Some(
                json!({"reference":format!("native-extend-{}-{seq}-{i}",self.nonce),"extraDurationMs":20000}),
            ),
            201,
        )?;
        self.update_booking_expiry(i, &row)
    }
    pub fn release(&mut self, _t: &dyn AuthorityHttpTransport) -> Result<()> {
        *self.deadline.lock().map_err(|_| "lease lock")? = None;
        self.lease
            .as_ref()
            .ok_or("no lease")?
            .release()
            .map_err(|_| "lease release")
    }
    pub fn validate_expected(&self, v: &Value) -> Result<()> {
        if v["kind"] != "exchange-connected-proof-expected-fresh" {
            return Err("expected kind rejected");
        }
        let b = self.bookings.as_ref().ok_or("no bookings")?;
        if v["bookingA"]["id"] != b[0] || v["bookingB"]["id"] != b[1] {
            return Err("expected booking mismatch");
        }
        for (field, expected) in [
            ("contributorOrg", self.binding.org),
            ("version", self.binding.version),
            ("publisherMachine", self.binding.publisher_machine),
            ("feed", self.feed),
            ("publisherInstance", self.publisher_instance),
        ] {
            if raw_identity(&v["shared"], field)? != expected {
                return Err("expected publisher mapping mismatch");
            }
        }
        let mut seen = std::collections::HashSet::new();
        for (section, keys) in [
            ("shared", ["feed", "version", "publisherMachine"]),
            ("bookingA", ["booking", "takerOrg", "receiverMachine"]),
            ("bookingB", ["booking", "takerOrg", "receiverMachine"]),
        ] {
            for key in keys {
                let s = text(&v[section], key)?;
                let bytes = URL_SAFE_NO_PAD
                    .decode(s)
                    .map_err(|_| "expected identity rejected")?;
                if bytes.len() != 16 || URL_SAFE_NO_PAD.encode(&bytes) != s || !seen.insert(bytes) {
                    return Err("expected identity rejected");
                }
            }
        }
        self.expected_validated
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

impl Bootstrap {
    pub fn generation(&self) -> Result<u32> {
        self.lease
            .as_ref()
            .and_then(|l| l.generation())
            .ok_or("no lease")
    }
    pub fn lease_valid(&self) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|l| now().ok().and_then(|n| l.permit(n)).is_some())
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline.lock().ok().and_then(|d| *d)
    }
    pub fn grant(
        &self,
        t: &dyn AuthorityHttpTransport,
        i: usize,
    ) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        if !self
            .expected_validated
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("expected mapping not validated");
        }
        if i >= 2 || !self.lease_valid() {
            return Err("lease expired");
        }
        let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
        let r = self.bounded_grant(t, i, b)?;
        if r.status != 200 {
            return Err("grant denied");
        }
        let directives: Vec<_> = r.cache_control.split(',').map(str::trim).collect();
        if r.body.len() != 264
            || r.content_length != Some(264)
            || !r
                .content_type
                .eq_ignore_ascii_case("application/octet-stream")
            || !directives
                .iter()
                .any(|s| s.eq_ignore_ascii_case("no-store"))
            || !directives.iter().any(|s| s.eq_ignore_ascii_case("private"))
            || directives.iter().any(|s| s.eq_ignore_ascii_case("public"))
        {
            return Err("grant metadata rejected");
        }
        let expiry = u64::from_be_bytes(r.body[160..168].try_into().map_err(|_| "grant expiry")?);
        if expiry <= now()?.saturating_add(1000) {
            return Err("grant near boundary");
        }
        Ok(r.body)
    }
    pub fn root(&self, t: &dyn AuthorityHttpTransport) -> Result<Value> {
        let _ = t; // The canonical provider owns the verified transport.
        let n = now()?;
        self.root_provider()?
            .current_epoch_root(&EpochRootRequest {
                org: self.binding.org,
                version: self.binding.version,
                feed: self.feed,
                publisher_machine: self.binding.publisher_machine,
                publisher: self.publisher_instance,
                epoch: n / 5000,
                now_ms: n,
            })
            .map_err(|_| "canonical root rejected")?;
        Ok(Value::Null)
    }
    /// Verify the released booking-lifetime route, not a synthetic exhaustion cause.
    /// The probe is admitted out of the epoch tail and clamped to the epoch end
    /// exactly like a real grant, so its 404 cannot be the near-boundary denial.
    /// One request, never retried.
    pub fn assert_expired_grant(&self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        if !self.profile.funded || now()? < self.booking_expiry()? {
            return Err("booking still live");
        }
        let wall = admitted_wall(self.admission_clock.as_ref())?;
        if wall < self.booking_expiry()? {
            return Err("booking still live");
        }
        let requested = wall
            .saturating_add(4000)
            .min(epoch_end(wall)?.saturating_sub(1));
        if requested <= wall.saturating_add(CLOCK_UNCERTAINTY_MS) {
            return Err("grant near boundary");
        }
        let b = &self.bookings.as_ref().ok_or("no bookings")?[0];
        let r = t
            .send_grant(
                AuthorityHttpRequest {
                    method: "POST",
                    url: self.booking_action(0, b, "grants")?,
                    body: Some(
                        serde_json::to_vec(
                            &json!({"machineId":self.takers[0].machine,"requestedExpiryMs":requested}),
                        )
                        .map_err(|_| "grant JSON")?,
                    ),
                    lease_headers: None,
                },
                &self.takers[0].csk,
            )
            .map_err(|_| "grant transport")?;
        if r.status != 404 {
            return Err("expired grant must return 404");
        }
        Ok(())
    }
    pub fn booking_release(&self, t: &dyn AuthorityHttpTransport, i: usize) -> Result<()> {
        let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
        Self::call(
            t,
            &self.takers[i],
            self.booking_action(i, b, "release")?,
            Some(json!({})),
            201,
        )
        .map(|_| ())
    }
    pub fn withdraw(&self, t: &dyn AuthorityHttpTransport, story: &str) -> Result<()> {
        let story = id(&json!({"story":story}), "story")?;
        Self::call(
            t,
            &self.contributor,
            format!(
                "{}/v0/organisations/{}/exchange/stories/{story}/withdraw",
                self.origin, self.contributor.org
            ),
            Some(json!({})),
            200,
        )
        .map(|_| ())
    }
    pub fn receipt(&self, t: &dyn AuthorityHttpTransport, i: usize, body: Value) -> Result<Value> {
        let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
        Self::call(
            t,
            &self.takers[i],
            self.booking_action(i, b, "receipts")?,
            Some(body),
            200,
        )
    }
}
