//! Private two-phase harness control plane. Never run on an output-clock thread.
//! Bootstrap grants are deliberately discarded: they are not identity evidence.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use conspect_media_crypto::authority_http::{
    AuthorityHttpRequest, AuthorityHttpTransport, OrganisationCsk,
};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

pub type Result<T> = std::result::Result<T, &'static str>;
const LIMIT: u64 = 65536;

/// Open first, then fstat. No symlinks, devices, hardlinks or permissive files.
pub fn private_json(path: &Path) -> Result<Value> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "private file unavailable")?;
    let m = f.metadata().map_err(|_| "private metadata unavailable")?;
    // SAFETY: geteuid has no pointer arguments or memory effects.
    if !m.is_file()
        || m.nlink() != 1
        || m.permissions().mode() & 0o7777 != 0o600
        || m.uid() != unsafe { libc::geteuid() }
        || m.len() > LIMIT
    {
        return Err("private file boundary rejected");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    (&mut f)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "private read failed")?;
    if bytes.len() as u64 > LIMIT {
        return Err("private file too large");
    }
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
fn integer(v: &Value, key: &str) -> Result<u64> {
    v.get(key)
        .and_then(Value::as_u64)
        .filter(|n| *n < (1 << 53))
        .ok_or("integer rejected")
}
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
/// No Debug: this owns bearer credentials and the in-process lease token.
pub struct Bootstrap {
    origin: String,
    version: String,
    contributor: Actor,
    takers: [Actor; 2],
    nonce: String,
    generation: Option<u32>,
    token: Zeroizing<String>,
    deadline: Option<Instant>,
    not_before: Option<u64>,
    profile: conspect_media_crypto::bootstrap::FixtureProfile,
    bookings: Option<[String; 2]>,
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
            generation: None,
            token: Zeroizing::new(String::new()),
            deadline: None,
            not_before: None,
            profile: conspect_media_crypto::bootstrap::FixtureProfile::parse(
                &v["scenario"]["fixtureProfile"],
            )?,
            bookings: None,
        })
    }
    fn version_path(&self, tail: &str) -> String {
        format!(
            "{}/v0/organisations/{}/exchange/versions/{}/publisher/{}",
            self.origin, self.contributor.org, self.version, tail
        )
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
    pub fn acquire(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        if self.generation.is_some() {
            return Err("lease already acquired");
        }
        let sent = Instant::now();
        let wall = now()?;
        let r = Self::call(
            t,
            &self.contributor,
            self.version_path("lease/acquire"),
            Some(json!({"machineId":self.contributor.machine,"bootNonce":self.nonce})),
            201,
        )?;
        let g = u32::try_from(integer(&r, "generation")?).map_err(|_| "generation rejected")?;
        let token = text(&r, "leaseToken")?;
        if URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| "lease token rejected")?
            .len()
            != 32
            || URL_SAFE_NO_PAD.encode(
                URL_SAFE_NO_PAD
                    .decode(token)
                    .map_err(|_| "lease token rejected")?,
            ) != token
        {
            return Err("lease token rejected");
        }
        let expiry = integer(&r, "leaseExpiresAtMs")?;
        let remaining = expiry
            .checked_sub(wall + 1250)
            .filter(|n| *n <= 10000)
            .ok_or("lease expired")?;
        self.deadline = Some(sent + Duration::from_millis(remaining));
        if Instant::now() >= self.deadline.unwrap() {
            return Err("lease expired");
        }
        self.not_before = Some(integer(&r, "notBeforeMs")?);
        self.generation = Some(g);
        self.token = Zeroizing::new(token.into());
        Ok(())
    }
    fn lease_body(&self) -> Result<Value> {
        Ok(
            json!({"machineId":self.contributor.machine,"bootNonce":self.nonce,"generation":self.generation.ok_or("no lease")?,"leaseToken":self.token.as_str()}),
        )
    }
    pub fn renew(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        if self.deadline.is_none_or(|d| Instant::now() >= d) {
            return Err("lease expired");
        }
        let sent = Instant::now();
        let wall = now()?;
        let r = Self::call(
            t,
            &self.contributor,
            self.version_path("lease/renew"),
            Some(self.lease_body()?),
            200,
        );
        // A failed renewal is terminal for this process; no acquire retry.
        self.deadline = None;
        let r = r?;
        if integer(&r, "generation")? != self.generation.unwrap() as u64
            || integer(&r, "notBeforeMs")? != self.not_before.unwrap()
        {
            return Err("lease fencing changed");
        }
        let remaining = integer(&r, "leaseExpiresAtMs")?
            .checked_sub(wall + 1250)
            .filter(|n| *n <= 10000)
            .ok_or("lease expired")?;
        self.deadline = Some(sent + Duration::from_millis(remaining));
        if Instant::now() >= self.deadline.unwrap() {
            self.deadline = None;
            return Err("lease expired");
        }
        Ok(())
    }
    pub fn book(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        if self.bookings.is_some() {
            return Err("already booked");
        }
        let mut bookings = Vec::new();
        for i in 0..2 {
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
            bookings.push(booking);
        }
        if bookings[0] == bookings[1] {
            return Err("aliased bookings");
        }
        self.bookings = Some(bookings.try_into().map_err(|_| "booking count")?);
        Ok(())
    }
    /// Mapping mint only: discard all grant bytes without using their identities.
    pub fn mint_mapping(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        use conspect_media_crypto::authority_http::LeaseHeaders;
        if self.deadline.is_none_or(|d| Instant::now() >= d) {
            return Err("lease expired");
        }
        if now()? < self.not_before.ok_or("no lease")?.saturating_add(1000) {
            return Err("takeover not ready; restart later");
        }
        let url = format!(
            "{}?machineId={}&generation={}",
            self.version_path("current-epoch"),
            self.contributor.machine,
            self.generation.ok_or("no lease")?
        );
        let r = t
            .send(
                AuthorityHttpRequest {
                    method: "GET",
                    url,
                    body: None,
                    lease_headers: Some(LeaseHeaders {
                        token: Zeroizing::new(self.token.to_string()),
                        boot_nonce: self.nonce.clone(),
                    }),
                },
                &self.contributor.csk,
            )
            .map_err(|_| "root transport rejected")?;
        let rbody = Zeroizing::new(r.body);
        if r.status != 200 {
            return Err("root status rejected");
        }
        let root: Value = serde_json::from_slice(&rbody).map_err(|_| "root JSON rejected")?;
        if root["versionId"] != self.version
            || root["publisherMachineId"] != self.contributor.machine
            || integer(&root, "generation")? != self.generation.unwrap() as u64
        {
            return Err("root context rejected");
        }
        for i in 0..2 {
            self.renew(t)?;
            let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
            let r=t.send_grant(AuthorityHttpRequest{method:"POST",url:self.booking_action(i,b,"grants")?,body:Some(serde_json::to_vec(&json!({"machineId":self.takers[i].machine,"requestedExpiryMs":now()?+4000})).map_err(|_| "grant JSON")?),lease_headers:None},&self.takers[i].csk).map_err(|_| "mapping grant transport rejected")?;
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
            if Instant::now() >= extend {
                seq += 1;
                for i in 0..2 {
                    let b = &self.bookings.as_ref().ok_or("no bookings")?[i];
                    Self::call(
                        t,
                        &self.takers[i],
                        self.booking_action(i, b, "extend")?,
                        Some(
                            json!({"reference":format!("extend-{}-{seq}-{i}",self.nonce),"extraDurationMs":20000}),
                        ),
                        201,
                    )?;
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
    pub fn release(&mut self, t: &dyn AuthorityHttpTransport) -> Result<()> {
        let body = self.lease_body()?;
        self.deadline = None;
        let result = Self::call(
            t,
            &self.contributor,
            self.version_path("lease/release"),
            Some(body),
            200,
        );
        self.token.clear();
        result.map(|_| ())
    }
    pub fn validate_expected(&self, v: &Value) -> Result<()> {
        if v["kind"] != "exchange-connected-proof-expected-fresh" {
            return Err("expected kind rejected");
        }
        let b = self.bookings.as_ref().ok_or("no bookings")?;
        if v["bookingA"]["id"] != b[0] || v["bookingB"]["id"] != b[1] {
            return Err("expected booking mismatch");
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
        Ok(())
    }
}
