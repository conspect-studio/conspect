//! Adapter tests for the frozen publisher authority contract, its exclusive
//! lease amendment and the opaque HTTP resource ids.
//!
//! Every test uses an injected transport and an injected clock: no live call is
//! made and no endpoint is assumed to exist. The opaque ids used here are
//! deliberately NOT the crypto identity, so any code that derives one from the
//! other fails these tests.
use base64::Engine as _;
use conspect_media_crypto::authority_http::{
    AuthorityHttpRequest, AuthorityHttpResponse, AuthorityHttpTransport, CryptoBinding,
    HttpAuthorityRootProvider, HttpResourceIds, LeaseClock, OrganisationCsk, PublisherLease,
};
use conspect_media_crypto::journal::DurableCounterJournal;
use conspect_media_crypto::{
    AuthorityRootGuard, AuthorityRootProvider, EpochRootRequest, Error, LeaseFence, ObjectId,
    Publisher,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

type Reply = conspect_media_crypto::Result<AuthorityHttpResponse>;

/// The opaque ids the authority issues: an `org_`-prefixed organisation id, an
/// exact version id string and an arbitrary `mch_` machine id.
const ORG_ID: &str = "org_12345678-0000-4000-8000-000000000000";
const VERSION_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
const MACHINE_ID: &str = "mch_abcdef0123456789";

/// The crypto identity registered for those opaque ids. Deliberately different
/// per field and NOT the UUID encoding of any opaque id.
const CRYPTO: CryptoBinding = CryptoBinding {
    org: [1; 16],
    version: [2; 16],
    publisher_machine: [3; 16],
};
const FEED: [u8; 16] = [4; 16];
const PUBLISHER: [u8; 16] = [5; 16];

struct Seen {
    method: String,
    url: String,
    body: Option<serde_json::Value>,
    lease_token: Option<String>,
    boot_nonce: Option<String>,
}

struct Mock {
    seen: Mutex<Vec<Seen>>,
    replies: Mutex<Vec<Reply>>,
}
impl Mock {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            seen: Mutex::new(vec![]),
            replies: Mutex::new(replies),
        }
    }
    fn calls(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
    fn call_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}
impl AuthorityHttpTransport for Mock {
    fn send(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
    ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
        // The credential is never printable, at any call.
        assert_eq!(format!("{credential:?}"), "OrganisationCsk(<redacted>)");
        let body = request.body.map(|v| serde_json::from_slice(&v).unwrap());
        let method = request.method;
        let url = request.url.clone();
        let (lease_token, boot_nonce) = match request.lease_headers {
            Some(headers) => (
                Some(headers.token.as_str().to_owned()),
                Some(headers.boot_nonce.clone()),
            ),
            None => (None, None),
        };
        self.seen.lock().unwrap().push(Seen {
            method: method.into(),
            url: url.clone(),
            body,
            lease_token,
            boot_nonce,
        });
        // An unexpected extra call finds an empty reply list and fails loudly,
        // which is how "no implicit retry" is enforced.
        let mut replies = self.replies.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !replies.is_empty(),
            "unexpected extra authority call: {method} {url}"
        );
        replies.remove(0)
    }
}

/// Injectable time: one shared millisecond counter for both clocks.
struct Clock {
    ms: AtomicU64,
}
impl Clock {
    fn new(start: u64) -> Arc<Self> {
        Arc::new(Self {
            ms: AtomicU64::new(start),
        })
    }
    fn set(&self, ms: u64) {
        self.ms.store(ms, Ordering::SeqCst);
    }
    fn lease_clock(self: &Arc<Self>) -> LeaseClock {
        let wall = self.clone();
        let monotonic = self.clone();
        LeaseClock {
            wall_ms: Arc::new(move || Ok(wall.ms.load(Ordering::SeqCst))),
            monotonic_ms: Arc::new(move || monotonic.ms.load(Ordering::SeqCst)),
        }
    }
    fn wall_fn(
        self: &Arc<Self>,
    ) -> Arc<dyn Fn() -> conspect_media_crypto::Result<u64> + Send + Sync> {
        let clock = self.clone();
        Arc::new(move || Ok(clock.ms.load(Ordering::SeqCst)))
    }
}

fn ids() -> HttpResourceIds {
    HttpResourceIds::new(ORG_ID, VERSION_ID, MACHINE_ID).unwrap()
}
fn custom_ids(org: &str, version: &str, machine: &str) -> HttpResourceIds {
    HttpResourceIds::new(org, version, machine).unwrap()
}
fn token() -> String {
    "t".repeat(43)
}
fn reply(status: u16, body: serde_json::Value) -> Reply {
    Ok(AuthorityHttpResponse {
        status,
        body: serde_json::to_vec(&body).unwrap(),
    })
}
/// The `201` acquire answer: the one place the contract hands out the token.
fn lease_body(generation: u32, expires: u64, not_before: u64) -> serde_json::Value {
    serde_json::json!({
        "generation": generation,
        "leaseToken": token(),
        "leaseExpiresAtMs": expires,
        "notBeforeMs": not_before,
    })
}
/// The frozen `200` renewal answer: metadata only, no token. A renewal never
/// rotates the token, so the client must retain the one it acquired.
fn renewed_lease_body(generation: u32, expires: u64, not_before: u64) -> serde_json::Value {
    serde_json::json!({
        "generation": generation,
        "leaseExpiresAtMs": expires,
        "notBeforeMs": not_before,
    })
}
/// A root/registration answer echoing the exact opaque ids.
fn root(generation: u64) -> serde_json::Value {
    serde_json::json!({"versionId":VERSION_ID, "publisherMachineId":MACHINE_ID,
        "generation":generation,
        "epoch":20,"epochStartMs":100000,"epochEndMs":105000,
        "rootKeyBase64":base64::engine::general_purpose::STANDARD.encode([7u8;32])})
}
fn request() -> EpochRootRequest {
    EpochRootRequest {
        org: CRYPTO.org,
        version: CRYPTO.version,
        feed: FEED,
        publisher_machine: CRYPTO.publisher_machine,
        publisher: PUBLISHER,
        epoch: 20,
        now_ms: 102001,
    }
}
/// The encoded prefix for the default ids.
fn prefix() -> String {
    format!(
        "https://authority.test/v0/organisations/{}/exchange/versions/{}/publisher",
        ORG_ID, VERSION_ID
    )
}
/// An owner-only temp dir suitable for a durable counter journal.
fn owner_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}
fn journal(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("nonce-journal.nr02")
}
fn object(number: u64) -> ObjectId {
    ObjectId {
        track: "video".into(),
        group: 1,
        object: number,
        captured_ms: 102001,
    }
}
fn parse(url: &str) -> reqwest::Url {
    reqwest::Url::parse(url).unwrap()
}
/// Decode the percent-escapes of one already-split path segment, so a test can
/// prove the exact opaque string survived encoding.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap();
            out.push(u8::from_str_radix(hex, 16).unwrap());
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn new_lease(mock: Arc<Mock>, clock: &Arc<Clock>) -> Arc<PublisherLease> {
    new_lease_with(mock, clock, ids(), CRYPTO)
}
fn new_lease_with(
    mock: Arc<Mock>,
    clock: &Arc<Clock>,
    ids: HttpResourceIds,
    binding: CryptoBinding,
) -> Arc<PublisherLease> {
    Arc::new(
        PublisherLease::with_transport(
            "https://authority.test",
            ids,
            binding,
            OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
            mock,
            clock.lease_clock(),
        )
        .unwrap()
        .with_margins(1000, 0),
    )
}

/// A provider with no initial generation: it takes the generation from the live
/// lease only, exactly as the amended contract requires.
fn provider(
    mock: Arc<Mock>,
    clock: &Arc<Clock>,
    lease: Arc<PublisherLease>,
) -> HttpAuthorityRootProvider {
    HttpAuthorityRootProvider::with_transport(
        "https://authority.test",
        ids(),
        CRYPTO,
        // Deliberately supplied and deliberately ignored: the amendment makes
        // the lease answer authoritative.
        Some(7),
        OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
        mock,
    )
    .unwrap()
    .with_clock(clock.wall_fn())
    .with_lease(lease)
}

fn provider_with(
    mock: Arc<Mock>,
    clock: &Arc<Clock>,
    ids: HttpResourceIds,
    binding: CryptoBinding,
    lease: Arc<PublisherLease>,
) -> HttpAuthorityRootProvider {
    HttpAuthorityRootProvider::with_transport(
        "https://authority.test",
        ids,
        binding,
        None,
        OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
        mock,
    )
    .unwrap()
    .with_clock(clock.wall_fn())
    .with_lease(lease)
}

/// The runtime's first step: acquire the lease (which assigns the generation)
/// and adopt that opaque value before any root request.
fn leased_provider(
    mock: Arc<Mock>,
    clock: &Arc<Clock>,
) -> (Arc<PublisherLease>, HttpAuthorityRootProvider) {
    let lease = new_lease(mock.clone(), clock);
    let assigned = lease.acquire().unwrap();
    let provider = provider(mock, clock, lease.clone());
    provider.adopt_generation(assigned).unwrap();
    (lease, provider)
}

fn protected_publisher(
    mock: Arc<Mock>,
    clock: &Arc<Clock>,
    dir: &tempfile::TempDir,
) -> (Arc<PublisherLease>, HttpAuthorityRootProvider, Publisher) {
    let (lease, provider) = leased_provider(mock, clock);
    clock.set(102001);
    let mut guard = AuthorityRootGuard::new().with_lease_fence(lease.clone());
    let publisher = Publisher::from_authority(
        &provider,
        &mut guard,
        CRYPTO.org,
        CRYPTO.version,
        FEED,
        CRYPTO.publisher_machine,
        PUBLISHER,
        102001,
        Box::new(DurableCounterJournal::open(&journal(dir)).unwrap()),
    )
    .unwrap();
    (lease, provider, publisher)
}

#[test]
fn first_acquire_assigns_the_generation_with_no_initial_guess() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
    ]));
    // A provider built with a caller-supplied generation ignores it.
    let lease = new_lease(mock.clone(), &clock);
    let provider = provider(mock.clone(), &clock, lease.clone());
    assert_eq!(provider.generation(), None, "no initial guess");
    assert_eq!(
        provider.current_epoch_root(&request()).unwrap_err(),
        Error::Expired
    );
    assert_eq!(mock.call_count(), 0);
    // The first acquire increments the generation and is authoritative.
    assert_eq!(lease.acquire().unwrap(), 1);
    provider.adopt_generation(1).unwrap();
    assert_eq!(provider.generation(), Some(1));
    assert_eq!(provider.adopt_generation(0).unwrap_err(), Error::Replay);
    assert!(provider.current_epoch_root(&request()).is_ok());
}

#[test]
fn acquire_renew_release_use_exact_routes_bodies_and_no_token_in_url() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 102001)),
        reply(200, renewed_lease_body(1, 122001, 102001)),
        reply(200, serde_json::json!({})),
        // The one explicit re-acquire after the release: the server still has
        // the lease record live, so it answers the contract's 409.
        reply(409, serde_json::json!({})),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    assert_eq!(lease.acquire().unwrap(), 1);
    assert_eq!(lease.generation(), Some(1));
    // The boot nonce is exactly the canonical unpadded base64url of 16 bytes.
    assert_eq!(lease.boot_nonce().len(), 22);
    assert!(!lease.boot_nonce().contains('='));
    assert_eq!(lease.renew().unwrap(), 1);
    lease.release().unwrap();
    assert_eq!(lease.generation(), None);
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    assert_eq!(lease.renew().unwrap_err(), Error::Denied);
    let calls = mock.calls();
    assert_eq!(calls.len(), 3);
    for call in &calls {
        // No secret ever appears in a URL or a lease header on lease calls.
        assert!(!call.url.contains("organisation-csk-secret"));
        assert!(!call.url.contains(&token()));
        assert!(call.lease_token.is_none() && call.boot_nonce.is_none());
        // The exact opaque ids, not a UUID encoding of the crypto binding.
        assert!(call.url.starts_with(&prefix()));
    }
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].url, format!("{}/lease/acquire", prefix()));
    let acquire_body = calls[0].body.as_ref().unwrap();
    assert_eq!(acquire_body["machineId"], MACHINE_ID);
    assert_eq!(acquire_body["bootNonce"], lease.boot_nonce());
    assert_eq!(acquire_body.as_object().unwrap().len(), 2);
    assert_eq!(calls[1].url, format!("{}/lease/renew", prefix()));
    let renew_body = calls[1].body.as_ref().unwrap();
    assert_eq!(renew_body["machineId"], MACHINE_ID);
    assert_eq!(renew_body["bootNonce"], lease.boot_nonce());
    assert_eq!(renew_body["leaseToken"], token());
    assert_eq!(renew_body["generation"], 1);
    assert_eq!(renew_body.as_object().unwrap().len(), 4);
    assert_eq!(calls[2].url, format!("{}/lease/release", prefix()));
    let release_body = calls[2].body.as_ref().unwrap();
    assert_eq!(release_body["generation"], 1);
    assert_eq!(release_body["leaseToken"], token());
    assert_eq!(release_body["machineId"], MACHINE_ID);
    // Once the local token is gone there is no implicit retry: exactly one
    // further acquire attempt is made, it is refused, and no token is installed.
    assert_eq!(lease.acquire().unwrap_err(), Error::Denied);
    assert_eq!(lease.generation(), None);
    let after = mock.calls();
    assert_eq!(after.len(), 1, "one explicit attempt, never a retry loop");
    assert_eq!(after[0].url, format!("{}/lease/acquire", prefix()));
    assert!(after[0].lease_token.is_none());
}

#[test]
fn root_fetch_requires_and_sends_the_lease_headers_exactly_once_after_not_before() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
    ]));
    let (lease, p) = leased_provider(mock.clone(), &clock);
    let context = p.current_epoch_root(&request()).unwrap();
    assert_eq!(context.epoch(), 20);
    assert_eq!(context.server_generation(), 1);
    assert!(!format!("{context:?}")
        .contains(&base64::engine::general_purpose::STANDARD.encode([7u8; 32])));
    let calls = mock.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].method, "GET");
    assert_eq!(
        calls[1].url,
        format!(
            "{}/current-epoch?machineId={}&generation=1",
            prefix(),
            MACHINE_ID
        )
    );
    assert!(calls[1].body.is_none(), "no GET body");
    assert_eq!(calls[1].lease_token.as_deref(), Some(token().as_str()));
    assert_eq!(calls[1].boot_nonce.as_deref(), Some(lease.boot_nonce()));
    let url = parse(&calls[1].url);
    assert_eq!(url.fragment(), None);
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("machineId".into(), MACHINE_ID.into()),
            ("generation".into(), "1".into())
        ]
    );
}

#[test]
fn opaque_ids_with_spaces_and_reserved_characters_are_percent_encoded() {
    let machine = "mch_ weird/id?x=1&y#z%q";
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(
            200,
            serde_json::json!({"versionId":VERSION_ID,
            "publisherMachineId":machine,"generation":1,"epoch":20,
            "epochStartMs":100000,"epochEndMs":105000,
            "rootKeyBase64":base64::engine::general_purpose::STANDARD.encode([7u8;32])}),
        ),
    ]));
    let lease = new_lease_with(
        mock.clone(),
        &clock,
        custom_ids(ORG_ID, VERSION_ID, machine),
        CRYPTO,
    );
    assert_eq!(lease.acquire().unwrap(), 1);
    // The acquire body carries the exact string, unencoded.
    let acquire = mock.calls();
    assert_eq!(acquire[0].body.as_ref().unwrap()["machineId"], machine);
    let p = provider_with(
        mock.clone(),
        &clock,
        custom_ids(ORG_ID, VERSION_ID, machine),
        CRYPTO,
        lease,
    );
    p.adopt_generation(1).unwrap();
    assert!(p.current_epoch_root(&request()).is_ok());
    let calls = mock.calls();
    let raw = &calls[0].url;
    let url = parse(raw);
    // One query parameter pair, decoded back to the exact opaque string.
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("machineId".into(), machine.into()),
            ("generation".into(), "1".into())
        ]
    );
    assert_eq!(url.fragment(), None);
    assert_eq!(
        url.path_segments().unwrap().count(),
        8,
        "fixed layout ending in current-epoch"
    );
    // The structural characters are encoded, not structural.
    assert!(raw.contains("%20"), "space encoded: {raw}");
    assert!(raw.contains("%2F") || raw.contains("%2f"), "slash encoded");
    assert!(!raw.contains('#'), "no literal fragment: {raw}");
    assert_eq!(raw.matches('?').count(), 1, "exactly one query: {raw}");
}

#[test]
fn a_space_or_plus_in_the_machine_id_round_trips_exactly() {
    // A space must not travel as `+` (a strict percent-decoding server would
    // read it literally), and a literal `+` must survive as well.
    let machine = "mch_+plus and space";
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(
            200,
            serde_json::json!({"versionId":VERSION_ID,"publisherMachineId":machine,
                "generation":1,"epoch":20,"epochStartMs":100000,"epochEndMs":105000,
                "rootKeyBase64":base64::engine::general_purpose::STANDARD.encode([7u8;32])}),
        ),
    ]));
    let ids = custom_ids(ORG_ID, VERSION_ID, machine);
    let lease = new_lease_with(mock.clone(), &clock, ids.clone(), CRYPTO);
    assert_eq!(lease.acquire().unwrap(), 1);
    let p = provider_with(mock.clone(), &clock, ids, CRYPTO, lease);
    p.adopt_generation(1).unwrap();
    assert!(p.current_epoch_root(&request()).is_ok());
    let calls = mock.calls();
    let raw = &calls[1].url;
    let url = parse(raw);
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("machineId".into(), machine.into()),
            ("generation".into(), "1".into())
        ]
    );
    assert!(raw.contains("%20"), "space as %20: {raw}");
    assert!(raw.contains("%2B"), "literal plus as %2B: {raw}");
    assert!(!raw.contains('+'), "no form-encoded space: {raw}");
}

#[test]
fn malicious_org_and_version_ids_cannot_escape_the_path_or_add_a_query() {
    let org = "org_../../admin";
    let version = "a/b?c=d#e;../..";
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![reply(201, lease_body(1, 112001, 101000))]));
    let lease = new_lease_with(
        mock.clone(),
        &clock,
        custom_ids(org, version, MACHINE_ID),
        CRYPTO,
    );
    assert_eq!(lease.acquire().unwrap(), 1);
    let calls = mock.calls();
    let url = parse(&calls[0].url);
    // The segment layout is fixed; each id is exactly one segment, and decoding
    // that segment returns the opaque string byte for byte.
    let segments: Vec<&str> = url.path_segments().unwrap().collect();
    assert_eq!(segments.len(), 9, "fixed layout: {segments:?}");
    assert_eq!(segments[0], "v0");
    assert_eq!(segments[1], "organisations");
    assert_eq!(percent_decode(segments[2]), org);
    assert_eq!(segments[3], "exchange");
    assert_eq!(segments[4], "versions");
    assert_eq!(percent_decode(segments[5]), version);
    assert_eq!(segments[6], "publisher");
    assert_eq!(segments[7], "lease");
    assert_eq!(segments[8], "acquire");
    // Nine segments means exactly nine slashes: no `/` from an id reached the
    // path, so nothing escaped or added a segment.
    assert_eq!(url.path().matches('/').count(), 9, "path: {}", url.path());
    assert_eq!(url.query(), None, "no id can introduce a query");
    assert_eq!(url.fragment(), None, "no id can introduce a fragment");
    assert!(url.path().starts_with("/v0/organisations/"));
    // The guarantee is at the encoded-URL level: every segment is atomic, so no
    // segment is a dot-segment and none contains a raw separator.
    for segment in &segments {
        assert_ne!(*segment, "..", "no dot-segment: {segments:?}");
        assert_ne!(*segment, ".", "no dot-segment: {segments:?}");
        assert!(!segment.contains('/'), "no raw separator: {segment}");
    }
}

#[test]
fn malicious_machine_id_cannot_inject_extra_query_parameters() {
    let machine = "mch_x&generation=999&machineId=other";
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(
            200,
            serde_json::json!({"versionId":VERSION_ID,"publisherMachineId":machine,
                "generation":1,"epoch":20,"epochStartMs":100000,"epochEndMs":105000,
                "rootKeyBase64":base64::engine::general_purpose::STANDARD.encode([7u8;32])}),
        ),
    ]));
    let lease = new_lease_with(
        mock.clone(),
        &clock,
        custom_ids(ORG_ID, VERSION_ID, machine),
        CRYPTO,
    );
    assert_eq!(lease.acquire().unwrap(), 1);
    let p = provider_with(
        mock.clone(),
        &clock,
        custom_ids(ORG_ID, VERSION_ID, machine),
        CRYPTO,
        lease,
    );
    p.adopt_generation(1).unwrap();
    assert!(p.current_epoch_root(&request()).is_ok());
    let calls = mock.calls();
    let url = parse(&calls[1].url);
    // Exactly the two expected pairs; the injected `generation=999` did not
    // become a parameter and the generation is the adopted one.
    assert_eq!(
        url.query_pairs().collect::<Vec<_>>(),
        vec![
            ("machineId".into(), machine.into()),
            ("generation".into(), "1".into())
        ]
    );
}

#[test]
fn exact_echo_mismatches_are_refused() {
    // A response that echoes a *derived* id (the crypto UUID hex) instead of the
    // opaque string is a different resource.
    let derived = "01020202-0202-0202-0202-020202020202";
    for (patch, error) in [
        (serde_json::json!({"versionId": derived}), Error::Denied),
        (
            serde_json::json!({"publisherMachineId": "mch_something-else"}),
            Error::Denied,
        ),
        (
            serde_json::json!({"versionId": VERSION_ID.to_uppercase()}),
            Error::Denied,
        ),
        (
            serde_json::json!({"publisherMachineId": format!(" {MACHINE_ID}")}),
            Error::Denied,
        ),
    ] {
        let clock = Clock::new(102001);
        let mut body = root(1);
        for (key, value) in patch.as_object().unwrap() {
            body[key] = value.clone();
        }
        let mock = Arc::new(Mock::new(vec![
            reply(201, lease_body(1, 112001, 101000)),
            reply(200, body),
        ]));
        let (_, p) = leased_provider(mock, &clock);
        assert_eq!(p.current_epoch_root(&request()).unwrap_err(), error);
    }
    // The lease answer's echoed identity must match exactly too.
    for body in [
        serde_json::json!({"generation":1,"leaseToken":token(),"leaseExpiresAtMs":112001,
            "notBeforeMs":101000,"machineId":"mch_other"}),
        serde_json::json!({"generation":1,"leaseToken":token(),"leaseExpiresAtMs":112001,
            "notBeforeMs":101000,"bootNonce":"A".repeat(22)}),
    ] {
        let clock = Clock::new(102001);
        let mock = Arc::new(Mock::new(vec![reply(201, body)]));
        let lease = new_lease(mock, &clock);
        assert_eq!(lease.acquire().unwrap_err(), Error::Denied);
        assert_eq!(lease.generation(), None);
    }
}

#[test]
fn crypto_identity_comes_from_the_registered_binding_not_the_opaque_ids() {
    // Opaque ids that are not UUIDs at all: nothing may be parsed out of them.
    let clock = Clock::new(102001);
    let dir = owner_dir();
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
    ]));
    let (_, provider, mut publisher) = protected_publisher(mock, &clock, &dir);
    // The sealed MC02 header carries the registered raw version, not any
    // encoding of VERSION_ID.
    let wire = publisher.encrypt(&object(1), 102002, b"one").unwrap();
    assert_eq!(&wire[..4], b"MC02");
    assert_eq!(&wire[4..20], &CRYPTO.version);
    assert_eq!(&wire[20..36], &FEED);
    assert_eq!(&wire[36..52], &PUBLISHER);
    // A request carrying the derived/other binding is refused before any call.
    let derived = EpochRootRequest {
        org: [9; 16],
        ..request()
    };
    assert_eq!(
        provider.current_epoch_root(&derived).unwrap_err(),
        Error::Denied
    );
}

#[test]
fn lease_and_provider_pairing_is_required_and_never_invented() {
    let clock = Clock::new(102001);
    // Same ids, different crypto binding: refused.
    let mock = Arc::new(Mock::new(vec![reply(201, lease_body(1, 112001, 101000))]));
    let lease = new_lease_with(
        mock.clone(),
        &clock,
        ids(),
        CryptoBinding {
            publisher_machine: [9; 16],
            ..CRYPTO
        },
    );
    lease.acquire().unwrap();
    let p = provider(mock.clone(), &clock, lease);
    assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Denied);
    assert_eq!(mock.call_count(), 1, "no root fetch for a mismatched pair");

    // Different opaque ids: refused.
    let mock = Arc::new(Mock::new(vec![reply(201, lease_body(1, 112001, 101000))]));
    let lease = new_lease_with(
        mock.clone(),
        &clock,
        custom_ids(ORG_ID, VERSION_ID, "mch_other"),
        CRYPTO,
    );
    lease.acquire().unwrap();
    let p = provider(mock.clone(), &clock, lease);
    assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Denied);
    assert_eq!(mock.call_count(), 1);
}

#[test]
fn register_and_rotation_use_the_opaque_ids_and_exact_echoes() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(
            201,
            serde_json::json!({"versionId":VERSION_ID,"publisherMachineId":MACHINE_ID,
                "generation":2}),
        ),
        reply(
            201,
            serde_json::json!({"versionId":VERSION_ID,"publisherMachineId":MACHINE_ID,
                "generation":3}),
        ),
    ]));
    let p = HttpAuthorityRootProvider::with_transport(
        "https://authority.test",
        ids(),
        CRYPTO,
        None,
        OrganisationCsk::new("csk".into()).unwrap(),
        mock.clone(),
    )
    .unwrap()
    .with_clock(clock.wall_fn());
    assert_eq!(p.register().unwrap(), 2);
    assert_eq!(p.generation(), Some(2));
    assert_eq!(p.rotate().unwrap(), 3);
    assert_eq!(p.generation(), Some(3));
    let calls = mock.calls();
    assert_eq!(calls[0].method, "POST");
    assert_eq!(calls[0].url, prefix());
    assert_eq!(calls[0].body.as_ref().unwrap()["machineId"], MACHINE_ID);
    assert_eq!(
        calls[0].body.as_ref().unwrap().as_object().unwrap().len(),
        1
    );
    assert_eq!(calls[1].url, format!("{}/rotation", prefix()));
    let rotate = calls[1].body.as_ref().unwrap();
    assert_eq!(rotate["machineId"], MACHINE_ID);
    assert_eq!(rotate["expectedGeneration"], 2);
    // A response that rewrites the opaque id is refused.
    let mock = Arc::new(Mock::new(vec![reply(
        201,
        serde_json::json!({"versionId":"other","publisherMachineId":MACHINE_ID,"generation":4}),
    )]));
    let p = HttpAuthorityRootProvider::with_transport(
        "https://authority.test",
        ids(),
        CRYPTO,
        None,
        OrganisationCsk::new("csk".into()).unwrap(),
        mock,
    )
    .unwrap();
    assert_eq!(p.register().unwrap_err(), Error::Denied);
    assert_eq!(p.generation(), None);
}

#[test]
fn id_validation_rejects_empty_control_and_unbounded_ids() {
    assert_eq!(
        HttpResourceIds::new("", VERSION_ID, MACHINE_ID),
        Err(Error::Denied)
    );
    assert_eq!(
        HttpResourceIds::new(ORG_ID, "", MACHINE_ID),
        Err(Error::Denied)
    );
    assert_eq!(
        HttpResourceIds::new(ORG_ID, VERSION_ID, ""),
        Err(Error::Denied)
    );
    for bad in ["a\nb", "a\rb", "a\tb", "a\0b", "a\u{1}b", "a\u{7f}b"] {
        assert_eq!(
            HttpResourceIds::new(ORG_ID, bad, MACHINE_ID),
            Err(Error::Denied),
            "control character must be refused"
        );
    }
    let long = "x".repeat(257);
    assert_eq!(
        HttpResourceIds::new(long, VERSION_ID, MACHINE_ID),
        Err(Error::Denied)
    );
    // At the bound it is accepted, and the string is preserved exactly.
    let at_bound = "x".repeat(256);
    let ok = HttpResourceIds::new(at_bound.clone(), VERSION_ID, MACHINE_ID).unwrap();
    assert_eq!(ok.org_id(), at_bound);
    // Non-ASCII is preserved and encoded later, not stripped.
    let unicode = HttpResourceIds::new(ORG_ID, "versão_ü", MACHINE_ID).unwrap();
    assert_eq!(unicode.version_id(), "versão_ü");
}

#[test]
fn no_lease_means_no_root_request_and_a_lost_token_never_renews() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![]));
    let bound = new_lease(mock.clone(), &clock);
    let p = HttpAuthorityRootProvider::with_transport(
        "https://authority.test",
        ids(),
        CRYPTO,
        None,
        OrganisationCsk::new("csk".into()).unwrap(),
        mock.clone(),
    )
    .unwrap()
    .with_clock(clock.wall_fn());
    assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Denied);
    assert!(mock.calls().is_empty());

    assert_eq!(bound.renew().unwrap_err(), Error::Denied);
    let holder = p.with_lease(bound);
    assert_eq!(
        holder.current_epoch_root(&request()).unwrap_err(),
        Error::Expired
    );
    assert!(mock.calls().is_empty(), "no wire call without a live lease");
}

#[test]
fn acquire_conflict_is_denied_once_and_never_remints_a_token() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![reply(409, serde_json::json!({}))]));
    let lease = new_lease(mock.clone(), &clock);
    assert_eq!(lease.acquire().unwrap_err(), Error::Denied);
    assert_eq!(lease.generation(), None);
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    assert_eq!(mock.call_count(), 1);
    assert_eq!(lease.renew().unwrap_err(), Error::Denied);
    assert_eq!(mock.call_count(), 1, "renew with no token makes no call");
}

#[test]
fn a_live_lease_makes_a_second_holder_and_a_lost_token_wait_for_expiry() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(4, 112001, 101000)),
        reply(409, serde_json::json!({})),
    ]));
    let host_a = new_lease(mock.clone(), &clock);
    let host_b = new_lease(mock.clone(), &clock);
    assert!(!host_a.boot_nonce().is_empty() && host_a.boot_nonce() != host_b.boot_nonce());
    assert_eq!(host_a.acquire().unwrap(), 4);
    assert_eq!(host_b.acquire().unwrap_err(), Error::Denied);
    assert_eq!(host_b.generation(), None);
    assert!(host_b.lease_headers().is_none());
    assert_eq!(host_a.generation(), Some(4));
    assert!(LeaseFence::permit(host_a.as_ref(), 102001).is_some());
    assert_eq!(mock.call_count(), 2);
}

#[test]
fn a_lease_required_or_stale_lease_answer_closes_the_local_fence() {
    for status in [409, 404] {
        let clock = Clock::new(102001);
        let mock = Arc::new(Mock::new(vec![
            reply(201, lease_body(1, 112001, 101000)),
            reply(status, serde_json::json!({})),
        ]));
        let (lease, p) = leased_provider(mock.clone(), &clock);
        assert!(LeaseFence::permit(lease.as_ref(), 102001).is_some());
        assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Denied);
        assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
        assert!(lease.lease_headers().is_none());
        assert_eq!(lease.renew().unwrap_err(), Error::Denied);
        assert_eq!(
            lease.revoked_by(),
            status,
            "the revoking status is recorded"
        );
        assert_eq!(mock.call_count(), 2, "one fetch, no retry after revocation");
        assert_eq!(
            p.current_epoch_root(&request()).unwrap_err(),
            Error::Expired
        );
        assert_eq!(mock.call_count(), 2);
    }
}

#[test]
fn expiry_renewal_failure_and_late_root_stop_sealing_locally() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        Err(Error::Denied),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    lease.acquire().unwrap();
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_some());
    assert!(lease.renew().is_err());
    assert!(LeaseFence::permit(lease.as_ref(), 105001).is_some());
    assert!(lease.needs_renewal(108001));
    clock.set(111001);
    assert!(LeaseFence::permit(lease.as_ref(), 111001).is_none());
    clock.set(112001);
    assert!(LeaseFence::permit(lease.as_ref(), 112001).is_none());

    let late = Clock::new(102001);
    let mock2 = Arc::new(Mock::new(vec![reply(201, lease_body(1, 112001, 101000))]));
    let lease2 = new_lease(mock2.clone(), &late);
    let assigned = lease2.acquire().unwrap();
    let p = provider(mock2.clone(), &late, lease2);
    p.adopt_generation(assigned).unwrap();
    late.set(111501);
    assert_eq!(
        p.current_epoch_root(&request()).unwrap_err(),
        Error::Expired
    );
    assert_eq!(
        mock2.call_count(),
        1,
        "the lease gate refused before any root fetch"
    );
}

#[test]
fn not_before_gate_blocks_a_root_until_the_previous_epoch_tail_ended() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![reply(201, lease_body(6, 132001, 107001))]));
    let lease = new_lease(mock.clone(), &clock);
    assert_eq!(lease.acquire().unwrap(), 6);
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    assert!(LeaseFence::permit(lease.as_ref(), 107001).is_none());
    let p = provider(mock.clone(), &clock, lease.clone());
    p.adopt_generation(6).unwrap();
    let mut late = request();
    late.now_ms = 104001;
    assert_eq!(p.current_epoch_root(&late).unwrap_err(), Error::Expired);
    assert_eq!(mock.call_count(), 1, "no root fetch before notBefore");
    clock.set(108001);
    assert!(LeaseFence::permit(lease.as_ref(), 108001).is_some());
}

#[test]
fn renewal_keeps_not_before_elapsed_so_a_live_lease_is_not_re_closed() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 102000)),
        reply(200, renewed_lease_body(1, 122001, 102000)),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    lease.acquire().unwrap();
    clock.set(103000);
    assert!(LeaseFence::permit(lease.as_ref(), 103000).is_some());
    lease.renew().unwrap();
    assert!(LeaseFence::permit(lease.as_ref(), 103000).is_some());
    let calls = mock.calls();
    assert_eq!(calls[1].body.as_ref().unwrap()["generation"], 1);
    // The renewal carried the retained token; the metadata answer carried none,
    // and the token retained is the acquired one.
    assert_eq!(calls[1].body.as_ref().unwrap()["leaseToken"], token());
    assert_eq!(
        lease.lease_headers().map(|(t, _)| t.as_str().to_owned()),
        Some(token())
    );
}

#[test]
fn generation_rollback_and_adoption_rollback_fail_closed() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(5, 112001, 102001)),
        reply(200, lease_body(4, 122001, 102001)),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    lease.acquire().unwrap();
    assert_eq!(lease.renew().unwrap_err(), Error::Replay);
    assert_eq!(lease.generation(), Some(5));

    let clock2 = Clock::new(102001);
    let mock2 = Arc::new(Mock::new(vec![reply(201, lease_body(2, 112001, 102001))]));
    let lease2 = new_lease(mock2.clone(), &clock2);
    assert_eq!(lease2.acquire().unwrap(), 2);
    let p = provider(mock2.clone(), &clock2, lease2.clone());
    assert_eq!(p.adopt_generation(2).unwrap(), ());
    assert_eq!(p.adopt_generation(1).unwrap_err(), Error::Replay);
    assert_eq!(p.generation(), Some(2));
    assert_eq!(p.adopt_generation(3).unwrap(), ());
    assert_eq!(p.generation(), Some(3));
    let before = mock2.call_count();
    assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Replay);
    assert_eq!(
        mock2.call_count(),
        before,
        "no fetch for a stale generation"
    );
}

#[test]
fn generation_overflow_and_unsafe_numbers_are_refused() {
    let clock = Clock::new(102001);
    for body in [
        serde_json::json!({"generation": 4294967296u64, "leaseToken": token(),
            "leaseExpiresAtMs": 112001, "notBeforeMs": 102001}),
        serde_json::json!({"generation": 2u64, "leaseToken": token(),
            "leaseExpiresAtMs": 9007199254740992u64, "notBeforeMs": 102001}),
        serde_json::json!({"generation": 2u64, "leaseToken": "short",
            "leaseExpiresAtMs": 112001, "notBeforeMs": 102001}),
        serde_json::json!({"generation": 2u64, "leaseToken": token(),
            "leaseExpiresAtMs": 102001, "notBeforeMs": 102001}),
        serde_json::json!({}),
    ] {
        let mock = Arc::new(Mock::new(vec![reply(201, body)]));
        let lease = new_lease(mock, &clock);
        assert!(lease.acquire().is_err());
        assert_eq!(lease.generation(), None);
    }
}

#[test]
fn a_released_lease_denies_encrypt_and_never_uses_the_old_key() {
    let clock = Clock::new(102001);
    let dir = owner_dir();
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
        reply(200, serde_json::json!({})),
    ]));
    let (lease, provider, mut publisher) = protected_publisher(mock.clone(), &clock, &dir);
    assert!(publisher.encrypt(&object(1), 102002, b"one").is_ok());
    lease.release().unwrap();
    assert_eq!(
        publisher.encrypt(&object(2), 102003, b"two").unwrap_err(),
        Error::Denied
    );
    assert!(provider.current_epoch_root(&request()).is_err());
}

#[test]
fn an_authority_root_without_a_fence_is_refused_at_seal_time() {
    let clock = Clock::new(102001);
    let dir = owner_dir();
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
    ]));
    let (_, provider) = leased_provider(mock, &clock);
    clock.set(102001);
    let mut guard = AuthorityRootGuard::new();
    let mut publisher = Publisher::from_authority(
        &provider,
        &mut guard,
        CRYPTO.org,
        CRYPTO.version,
        FEED,
        CRYPTO.publisher_machine,
        PUBLISHER,
        102001,
        Box::new(DurableCounterJournal::open(&journal(&dir)).unwrap()),
    )
    .unwrap();
    assert_eq!(
        publisher.encrypt(&object(1), 102002, b"x").unwrap_err(),
        Error::Denied
    );
}

#[test]
fn new_process_gets_a_fresh_boot_nonce_and_never_reuses_a_persisted_one() {
    let first = new_lease(Arc::new(Mock::new(vec![])), &Clock::new(102001));
    let second = new_lease(Arc::new(Mock::new(vec![])), &Clock::new(102001));
    assert_ne!(first.boot_nonce(), second.boot_nonce());
    assert_eq!(first.boot_nonce().len(), 22);
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(first.boot_nonce())
        .unwrap();
    assert_eq!(decoded.len(), 16);
    assert_eq!(second.generation(), None);
    assert!(second.lease_headers().is_none());
    assert!(LeaseFence::permit(second.as_ref(), 102001).is_none());
}

#[test]
fn durable_counter_is_retained_across_a_lease_renewal_and_restart() {
    let clock = Clock::new(102001);
    let dir = owner_dir();
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
        // Metadata-only renewal: the retained token must keep sealing and the
        // durable counter must stay continuous across it.
        reply(200, renewed_lease_body(1, 122001, 101000)),
        reply(200, root(1)),
    ]));
    let (lease, provider, mut publisher) = protected_publisher(mock.clone(), &clock, &dir);
    let first = publisher.encrypt(&object(1), 102002, b"one").unwrap();
    assert_eq!(u64::from_be_bytes(first[64..72].try_into().unwrap()), 0);
    assert_eq!(lease.renew().unwrap(), 1);
    assert!(publisher.encrypt(&object(2), 102003, b"two").is_ok());
    drop(publisher);
    clock.set(102004);
    let mut guard = AuthorityRootGuard::new().with_lease_fence(lease.clone());
    let mut second = Publisher::from_authority(
        &provider,
        &mut guard,
        CRYPTO.org,
        CRYPTO.version,
        FEED,
        CRYPTO.publisher_machine,
        PUBLISHER,
        102004,
        Box::new(DurableCounterJournal::open(&journal(&dir)).unwrap()),
    )
    .unwrap();
    let next = second.encrypt(&object(3), 102004, b"three").unwrap();
    assert_eq!(u64::from_be_bytes(next[64..72].try_into().unwrap()), 2);
}

#[test]
fn tokenless_renewal_keeps_the_token_generation_and_sealed_counter() {
    let clock = Clock::new(102001);
    let dir = owner_dir();
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        reply(200, root(1)),
        // The frozen renewal answer carries no token at all.
        reply(200, renewed_lease_body(1, 122001, 101000)),
    ]));
    let (lease, _provider, mut publisher) = protected_publisher(mock.clone(), &clock, &dir);
    let first = publisher.encrypt(&object(1), 102002, b"one").unwrap();
    assert_eq!(&first[..4], b"MC02");
    assert_eq!(u64::from_be_bytes(first[64..72].try_into().unwrap()), 0);

    assert_eq!(lease.renew().unwrap(), 1, "the generation is unchanged");
    assert_eq!(
        lease.generation(),
        Some(1),
        "a tokenless renewal must not drop the lease"
    );
    assert_eq!(
        lease.lease_headers().map(|(t, _)| t.as_str().to_owned()),
        Some(token()),
        "the acquired token is retained verbatim"
    );
    assert!(LeaseFence::permit(lease.as_ref(), 102002).is_some());

    // Sealing still works after the tokenless renewal, and the durable counter
    // continues rather than restarting.
    let second = publisher.encrypt(&object(2), 102003, b"two").unwrap();
    assert_eq!(&second[..4], b"MC02");
    assert_eq!(u64::from_be_bytes(second[64..72].try_into().unwrap()), 1);

    let calls = mock.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].url, format!("{}/lease/renew", prefix()));
    assert_eq!(calls[2].body.as_ref().unwrap()["leaseToken"], token());
    assert_eq!(calls[2].body.as_ref().unwrap()["generation"], 1);
    // No secret is ever placed in the URL of the renewal.
    assert!(!calls[2].url.contains(&token()));
}

#[test]
fn a_renewal_carrying_the_same_token_is_tolerated_but_changes_nothing() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        // A loose server re-echoes the token it was sent: tolerated, and the
        // retained value is still exactly the acquired one.
        reply(200, lease_body(1, 122001, 101000)),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    assert_eq!(lease.acquire().unwrap(), 1);
    assert_eq!(lease.renew().unwrap(), 1);
    assert_eq!(
        lease.lease_headers().map(|(t, _)| t.as_str().to_owned()),
        Some(token()),
        "a re-echoed identical token is not a rotation"
    );
    assert!(LeaseFence::permit(lease.as_ref(), 102002).is_some());
}

#[test]
fn a_renewal_that_rotates_the_token_latches_the_fence_closed() {
    // R2: a divergent token on renew is authority equivocation. The publisher
    // must stop sealing at once, exactly like a root-route 409/404: the fence
    // latches, the held token is zeroized, the rotated value is never adopted,
    // and nothing further (renew, root, re-acquire) goes on the wire.
    let clock = Clock::new(102001);
    let rotated = "R".repeat(43);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(5, 112001, 101000)),
        reply(
            200,
            serde_json::json!({"generation":5,"leaseToken":rotated,
                "leaseExpiresAtMs":122001,"notBeforeMs":101000}),
        ),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    assert_eq!(lease.acquire().unwrap(), 5);
    assert!(LeaseFence::permit(lease.as_ref(), 102002).is_some());
    assert_eq!(lease.renew().unwrap_err(), Error::Denied);
    assert_eq!(lease.generation(), None, "the lease is latched dead");
    assert!(lease.lease_headers().is_none(), "no token survives");
    assert!(LeaseFence::permit(lease.as_ref(), 102002).is_none());
    // Nothing more reaches the wire: the mock has no further replies and would
    // panic on an extra call.
    assert_eq!(lease.renew().unwrap_err(), Error::Denied);
    lease.release().unwrap();
    let p = provider(mock.clone(), &clock, lease.clone());
    assert!(p.current_epoch_root(&request()).is_err());
    let calls = mock.calls();
    assert_eq!(calls.len(), 2, "acquire and the refused renewal only");
    assert_eq!(calls[1].url, format!("{}/lease/renew", prefix()));
    assert_eq!(calls[1].body.as_ref().unwrap()["leaseToken"], token());
}

/// A transport that parks the renew answer until the test lets it land, so a
/// release/revoke can run while the renew is on the wire (R1).
struct Gated {
    inner: Mock,
    /// The lease route whose answer is parked (`/lease/renew` or `/lease/acquire`).
    parked: &'static str,
    entered: std::sync::mpsc::SyncSender<()>,
    go: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl AuthorityHttpTransport for Gated {
    fn send(
        &self,
        request: AuthorityHttpRequest,
        credential: &OrganisationCsk,
    ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
        let parked = request.url.ends_with(self.parked);
        let answer = self.inner.send(request, credential);
        if parked {
            self.entered.send(()).unwrap();
            self.go.lock().unwrap().recv().unwrap();
        }
        answer
    }
}

fn race_release_against_an_in_flight_renew(use_revoke: bool) {
    let clock = Clock::new(102001);
    let mut replies = vec![
        reply(201, lease_body(7, 112001, 101000)),
        // A perfectly valid renewal: it must still not resurrect the lease.
        reply(200, renewed_lease_body(7, 122001, 101000)),
    ];
    if !use_revoke {
        replies.push(reply(200, serde_json::json!({})));
    }
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (go_tx, go_rx) = std::sync::mpsc::sync_channel(1);
    let gated = Arc::new(Gated {
        inner: Mock::new(replies),
        parked: "/lease/renew",
        entered: entered_tx,
        go: Mutex::new(go_rx),
    });
    let lease = Arc::new(
        PublisherLease::with_transport(
            "https://authority.test",
            ids(),
            CRYPTO,
            OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
            gated.clone(),
            clock.lease_clock(),
        )
        .unwrap(),
    );
    assert_eq!(lease.acquire().unwrap(), 7);
    let renewer = {
        let lease = lease.clone();
        std::thread::spawn(move || lease.renew())
    };
    entered_rx.recv().unwrap();
    let releaser = if use_revoke {
        lease.revoke();
        None
    } else {
        let releasing = lease.clone();
        let handle = std::thread::spawn(move || releasing.release());
        // release() latches locally before it waits for the wire serializer.
        while lease.generation().is_some() {
            std::thread::yield_now();
        }
        Some(handle)
    };
    // The fence is already closed while the renew is still on the wire.
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    go_tx.send(()).unwrap();
    assert_eq!(renewer.join().unwrap().unwrap_err(), Error::Denied);
    if let Some(handle) = releaser {
        handle.join().unwrap().unwrap();
    }
    // Not resurrected: no generation, no token, no permit, no further renewal.
    assert_eq!(lease.generation(), None);
    assert!(lease.lease_headers().is_none());
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    assert_eq!(lease.renew().unwrap_err(), Error::Denied);
    let calls = gated.inner.calls();
    let routes: Vec<_> = calls
        .iter()
        .map(|c| c.url.rsplit('/').next().unwrap().to_owned())
        .collect();
    if use_revoke {
        assert_eq!(routes, ["acquire", "renew"]);
    } else {
        // The release is sent once, after the in-flight renew, never interleaved.
        assert_eq!(routes, ["acquire", "renew", "release"]);
        assert_eq!(calls[2].body.as_ref().unwrap()["leaseToken"], token());
    }
}

#[test]
fn release_racing_an_in_flight_acquire_never_installs_the_lease() {
    // An acquire only starts while released, so release() takes its early path.
    // That path must still invalidate the in-flight acquire's answer.
    let clock = Clock::new(102001);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (go_tx, go_rx) = std::sync::mpsc::sync_channel(1);
    let gated = Arc::new(Gated {
        inner: Mock::new(vec![reply(201, lease_body(9, 112001, 101000))]),
        parked: "/lease/acquire",
        entered: entered_tx,
        go: Mutex::new(go_rx),
    });
    let lease = Arc::new(
        PublisherLease::with_transport(
            "https://authority.test",
            ids(),
            CRYPTO,
            OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
            gated.clone(),
            clock.lease_clock(),
        )
        .unwrap(),
    );
    let acquirer = {
        let lease = lease.clone();
        std::thread::spawn(move || lease.acquire())
    };
    entered_rx.recv().unwrap();
    lease.release().unwrap();
    go_tx.send(()).unwrap();
    assert_eq!(acquirer.join().unwrap().unwrap_err(), Error::Denied);
    assert_eq!(lease.generation(), None);
    assert!(lease.lease_headers().is_none());
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    assert_eq!(lease.renew().unwrap_err(), Error::Denied);
    // Only the acquire reached the wire: no release (nothing was held) and no retry.
    assert_eq!(gated.inner.call_count(), 1);
}

#[test]
fn release_racing_an_in_flight_renew_never_resurrects_the_lease() {
    race_release_against_an_in_flight_renew(false);
}

#[test]
fn revoke_racing_an_in_flight_renew_never_resurrects_the_lease() {
    race_release_against_an_in_flight_renew(true);
}

#[test]
fn an_acquire_without_a_token_is_refused() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        // The acquire answer is the only place the contract mints a token.
        reply(201, renewed_lease_body(1, 112001, 101000)),
    ]));
    let lease = new_lease(mock.clone(), &clock);
    assert_eq!(lease.acquire().unwrap_err(), Error::Invalid);
    assert_eq!(lease.generation(), None);
    assert!(lease.lease_headers().is_none());
    assert!(LeaseFence::permit(lease.as_ref(), 102001).is_none());
    assert_eq!(
        mock.call_count(),
        1,
        "no retry that could mint a second token"
    );
}

#[test]
fn mismatched_identity_generation_expiry_base64_and_size_fail_closed() {
    let clock = Clock::new(102001);
    let cases = [
        (serde_json::json!({"versionId":"wrong"}), Error::Denied),
        (serde_json::json!({"generation":3}), Error::Replay),
        (serde_json::json!({"epochEndMs":99999}), Error::Expired),
        (serde_json::json!({"rootKeyBase64":"!!!!"}), Error::Invalid),
        (
            serde_json::json!({"generation":4294967296u64}),
            Error::GenerationOverflow,
        ),
        (
            serde_json::json!({"epochStartMs":9007199254740992u64}),
            Error::Invalid,
        ),
    ];
    for (patch, error) in cases {
        let mut body = root(1);
        for (key, value) in patch.as_object().unwrap() {
            body[key] = value.clone();
        }
        let mock = Arc::new(Mock::new(vec![
            reply(201, lease_body(1, 112001, 101000)),
            reply(200, body),
        ]));
        let (_, p) = leased_provider(mock, &clock);
        assert_eq!(p.current_epoch_root(&request()).unwrap_err(), error);
    }
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        Ok(AuthorityHttpResponse {
            status: 200,
            body: vec![b'x'; 2049],
        }),
    ]));
    let (_, p) = leased_provider(mock, &clock);
    assert_eq!(
        p.current_epoch_root(&request()).unwrap_err(),
        Error::Capacity
    );
}

#[test]
fn non_success_and_unsafe_origin_never_return_local_root() {
    let clock = Clock::new(102001);
    for status in [301, 302, 401, 403, 503] {
        let mock = Arc::new(Mock::new(vec![
            reply(201, lease_body(1, 112001, 101000)),
            reply(status, root(1)),
        ]));
        let (_, p) = leased_provider(mock, &clock);
        assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Denied);
    }
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(1, 112001, 101000)),
        Err(Error::Denied),
    ]));
    let (_, p) = leased_provider(mock, &clock);
    assert_eq!(p.current_epoch_root(&request()).unwrap_err(), Error::Denied);

    for origin in [
        "http://authority.test",
        "https://user:pass@authority.test",
        "https://authority.test/path",
        "https://authority.test/?secret=x",
    ] {
        assert!(HttpAuthorityRootProvider::with_transport(
            origin,
            ids(),
            CRYPTO,
            Some(2),
            OrganisationCsk::new("secret".into()).unwrap(),
            Arc::new(Mock::new(vec![]))
        )
        .is_err());
        assert!(PublisherLease::with_transport(
            origin,
            ids(),
            CRYPTO,
            OrganisationCsk::new("secret".into()).unwrap(),
            Arc::new(Mock::new(vec![])),
            clock.lease_clock(),
        )
        .is_err());
    }
    assert!(OrganisationCsk::new(String::new()).is_err());
}

#[test]
fn the_fence_is_local_and_performs_no_io() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![reply(201, lease_body(1, 112001, 101000))]));
    let lease = new_lease(mock.clone(), &clock);
    lease.acquire().unwrap();
    let before = mock.call_count();
    for _ in 0..100 {
        assert!(LeaseFence::permit(lease.as_ref(), 102001).is_some());
    }
    assert_eq!(mock.call_count(), before, "the fence performs no I/O");
}

#[test]
fn live_lease_generation_is_adopted_even_when_root_fetch_fails() {
    let clock = Clock::new(102001);
    let mock = Arc::new(Mock::new(vec![
        reply(201, lease_body(7, 112001, 101000)),
        reply(503, serde_json::json!({})),
    ]));
    let ids = custom_ids(ORG_ID, VERSION_ID, MACHINE_ID);
    let lease = new_lease_with(mock.clone(), &clock, ids.clone(), CRYPTO);
    assert_eq!(lease.acquire().unwrap(), 7);
    let p = provider_with(mock, &clock, ids, CRYPTO, lease);
    assert_eq!(p.generation(), None);
    assert!(p.current_epoch_root(&request()).is_err());
    assert_eq!(p.generation(), Some(7));
    assert!(p.adopt_generation(6).is_err());
}

/// Review finding 2 of PR #312: a lease that closes while the root answer is in
/// flight must be a refusal (`Denied`), never `Expired`, even when the answer
/// also arrives after the epoch boundary. `Expired` is the only error a caller
/// may treat as an answer overtaken in transit; a closed fence must not look
/// like one. The release happens INSIDE the transport call, i.e. between the
/// request leaving and its answer arriving.
#[test]
fn a_lease_closed_while_the_root_answer_is_in_flight_is_denied_not_expired() {
    struct CloseInFlight {
        inner: Arc<Mock>,
        lease: Mutex<Option<Arc<PublisherLease>>>,
        clock: Arc<Clock>,
        answer_at: u64,
    }
    impl AuthorityHttpTransport for CloseInFlight {
        fn send(
            &self,
            request: AuthorityHttpRequest,
            credential: &OrganisationCsk,
        ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
            let root = request.url.contains("/current-epoch");
            let answer = self.inner.send(request, credential);
            if root {
                // The answer is in flight: the fence closes, then it arrives
                // (optionally after the epoch boundary as well).
                self.lease.lock().unwrap().as_ref().unwrap().revoke();
                self.clock.set(self.answer_at);
            }
            answer
        }
    }
    // Same epoch, and across the boundary: both must be Denied.
    for answer_at in [102501, 105001] {
        let clock = Clock::new(102001);
        let inner = Arc::new(Mock::new(vec![
            reply(201, lease_body(1, 112001, 101000)),
            reply(200, root(1)),
        ]));
        let transport = Arc::new(CloseInFlight {
            inner: inner.clone(),
            lease: Mutex::new(None),
            clock: clock.clone(),
            answer_at,
        });
        let lease = Arc::new(
            PublisherLease::with_transport(
                "https://authority.test",
                ids(),
                CRYPTO,
                OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
                transport.clone(),
                clock.lease_clock(),
            )
            .unwrap()
            .with_margins(1000, 0),
        );
        let assigned = lease.acquire().unwrap();
        *transport.lease.lock().unwrap() = Some(lease.clone());
        let p = HttpAuthorityRootProvider::with_transport(
            "https://authority.test",
            ids(),
            CRYPTO,
            None,
            OrganisationCsk::new("organisation-csk-secret".into()).unwrap(),
            transport,
        )
        .unwrap()
        .with_clock(clock.wall_fn())
        .with_lease(lease);
        p.adopt_generation(assigned).unwrap();
        assert_eq!(
            p.current_epoch_root(&request()).unwrap_err(),
            Error::Denied,
            "answer at {answer_at}"
        );
        assert_eq!(inner.call_count(), 2, "one root request, no retry");
    }
}
