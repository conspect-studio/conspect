use base64::Engine;
use conspect_media_crypto::authority_http::VerifiedHttpsTransport;
use conspect_media_crypto::bootstrap as harness;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::{pki_types::PrivatePkcs8KeyDer, ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
};
fn manifest(origin: &str) -> Value {
    json!({"kind":"exchange-connected-proof-harness","baseUrl":origin,"basePath":"/v0","scenario":{"expectedIdentities":{"shared":{"feed":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([9;16]),"version":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([3;16]),"publisherMachine":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([5;16]),"contributorOrg":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([10;16]),"publisherInstance":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([5;16])}},"versionId":"ver_test","publisherLease":{"bootstrapOnly":true,"adoptable":false}},"credentials":{"contributor":{"orgId":"org_pub","machineId":"mac_pub","token":"synthetic-pub"},"takerA":{"orgId":"org_a","machineId":"mac_a","token":"synthetic-a"},"takerB":{"orgId":"org_b","machineId":"mac_b","token":"synthetic-b"}}})
}
#[test]
fn fixture_profile_defaults_and_validation() {
    use harness::FixtureProfile;
    for v in [
        Value::Null,
        json!({"name":"standard","bookingDurationMs":10000}),
    ] {
        let p = FixtureProfile::parse(&v).unwrap();
        assert_eq!(p.booking_duration_ms(0), 60000);
        assert_eq!(p.booking_duration_ms(1), 60000);
        assert!(!p.funded_expiry());
    }
    let p = FixtureProfile::parse(
        &json!({"name":"funded-expiry","takerAAvailableMicro":"900719925474099300000"}),
    )
    .unwrap();
    assert_eq!(p.booking_duration_ms(0), 10000);
    assert_eq!(p.booking_duration_ms(1), 60000);
    for invalid in [
        json!({"name":"unknown"}),
        json!({"name":"funded-expiry","bookingDurationMs":0}),
        json!({"name":"funded-expiry","extensionDueAfterBookingMs":10000}),
        json!({"name":"funded-expiry","expectedExtensionCause":"expired"}),
        json!({"name":"funded-expiry","expectedExtensionStatus":201}),
        json!({"name":"funded-expiry","takerAAvailableMicro":100000}),
    ] {
        assert!(FixtureProfile::parse(&invalid).is_err(), "{invalid}");
    }
}

#[test]
fn funded_expiry_books_a_short_and_expects_only_credit_refusal() {
    use conspect_media_crypto::authority_http::{
        AuthorityHttpRequest, AuthorityHttpResponse, AuthorityHttpTransport, OrganisationCsk,
    };
    use std::sync::Mutex;
    struct Mock {
        calls: Mutex<Vec<Value>>,
        wrong: bool,
    }
    impl AuthorityHttpTransport for Mock {
        fn send(
            &self,
            r: AuthorityHttpRequest,
            _: &OrganisationCsk,
        ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
            let body: Value = serde_json::from_slice(r.body.as_ref().unwrap()).unwrap();
            self.calls.lock().unwrap().push(body.clone());
            let org = if r.url.contains("org_a/") {
                "org_a"
            } else {
                "org_b"
            };
            let (status, reply) = if r.url.ends_with("/bookings") {
                (201, {
                    let wall = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    let duration = body["durationMs"].as_u64().unwrap();
                    // The real 201 DTO: snake_case Unix-ms integers, no camelCase alias.
                    json!({"id":format!("bk:{org}:{}",body["reference"].as_str().unwrap()),"created_at":wall,"expires_at":wall+duration})
                })
            } else if org == "org_a" {
                assert_eq!(body["extraDurationMs"], 5000);
                (
                    409,
                    json!({"type":"about:blank","title":"Conflict","status":409,"detail":if self.wrong {"The booking has expired."} else {"The prepaid exchange balance cannot cover the additional reservation; the expiry is unchanged."}}),
                )
            } else {
                assert_eq!(body["extraDurationMs"], 20000);
                let wall = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                (201, json!({"expires_at":wall + 80000}))
            };
            Ok(AuthorityHttpResponse {
                status,
                body: serde_json::to_vec(&reply).unwrap(),
            })
        }
    }
    for wrong in [false, true] {
        let mut m = manifest("https://[::1]/");
        m["scenario"]["fixtureProfile"] = json!({"name":"funded-expiry"});
        let mut client = harness::Bootstrap::parse(&m).unwrap();
        let t = Mock {
            calls: Mutex::new(Vec::new()),
            wrong,
        };
        client.book(&t).unwrap();
        assert_eq!(t.calls.lock().unwrap()[0]["durationMs"], 10000);
        assert_eq!(t.calls.lock().unwrap()[1]["durationMs"], 60000);
        assert_eq!(client.extend_booking(&t, 0, 1).is_err(), wrong);
        client.extend_booking(&t, 1, 1).unwrap();
    }
}

#[test]
fn standard_profile_requests_are_bit_for_bit_unchanged() {
    use conspect_media_crypto::authority_http::{
        AuthorityHttpRequest, AuthorityHttpResponse, AuthorityHttpTransport, OrganisationCsk,
    };
    use std::sync::Mutex;
    struct Mock(Mutex<Vec<(String, Vec<u8>)>>);
    impl AuthorityHttpTransport for Mock {
        fn send(
            &self,
            r: AuthorityHttpRequest,
            _: &OrganisationCsk,
        ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
            let raw = r.body.clone().unwrap();
            let body: Value = serde_json::from_slice(&raw).unwrap();
            self.0.lock().unwrap().push((r.url.clone(), raw));
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let org = if r.url.contains("org_a/") {
                "org_a"
            } else {
                "org_b"
            };
            let reply = if r.url.ends_with("/bookings") {
                json!({"id":format!("bk:{org}:{}",body["reference"].as_str().unwrap()),"created_at":wall,"expires_at":wall+60000})
            } else {
                json!({"expires_at":wall+80000})
            };
            Ok(AuthorityHttpResponse {
                status: 201,
                body: serde_json::to_vec(&reply).unwrap(),
            })
        }
    }
    let mut seen = Vec::new();
    for profile in [None, Some(json!({"name":"standard"}))] {
        let mut m = manifest("https://[::1]/");
        if let Some(p) = profile {
            m["scenario"]["fixtureProfile"] = p;
        }
        let mut client = harness::Bootstrap::parse(&m).unwrap();
        assert!(!client.funded_expiry());
        let t = Mock(Mutex::new(Vec::new()));
        client.book(&t).unwrap();
        client.extend_booking(&t, 0, 1).unwrap();
        client.extend_booking(&t, 1, 1).unwrap();
        let calls = t.0.into_inner().unwrap();
        // Exact request bytes, with only the per-process random nonce masked.
        let nonce = calls[0].1.clone();
        let nonce: Value = serde_json::from_slice(&nonce).unwrap();
        let nonce = nonce["reference"].as_str().unwrap()["engine-".len()..]
            .trim_end_matches("-0")
            .to_owned();
        let masked: Vec<String> = calls
            .iter()
            .map(|(u, b)| {
                format!("{u} {}", String::from_utf8(b.clone()).unwrap()).replace(&nonce, "N")
            })
            .collect();
        assert_eq!(
            masked,
            [
                r#"https://[::1]/v0/organisations/org_a/exchange/bookings {"durationMs":60000,"machineId":"mac_a","reference":"engine-N-0","versionId":"ver_test"}"#,
                r#"https://[::1]/v0/organisations/org_b/exchange/bookings {"durationMs":60000,"machineId":"mac_b","reference":"engine-N-1","versionId":"ver_test"}"#,
                r#"https://[::1]/v0/organisations/org_a/exchange/bookings/bk:org_a:engine-N-0/extend {"extraDurationMs":20000,"reference":"native-extend-N-1-0"}"#,
                r#"https://[::1]/v0/organisations/org_b/exchange/bookings/bk:org_b:engine-N-1/extend {"extraDurationMs":20000,"reference":"native-extend-N-1-1"}"#,
            ]
        );
        // The standard first extension stays 15 s out, not the funded 3 s.
        assert!(
            client.extension_due(0).unwrap()
                > std::time::Instant::now() + std::time::Duration::from_secs(14)
        );
        seen.push(masked);
    }
    assert_eq!(
        seen[0], seen[1],
        "absent profile and \"standard\" are identical"
    );
}

#[test]
fn rejects_stale_active_lease_and_untrusted_origin() {
    let mut m = manifest("https://[::1]:23456/");
    assert!(harness::Bootstrap::parse(&m).is_ok());
    m["scenario"]["publisherLease"]["generation"] = json!(793);
    assert!(harness::Bootstrap::parse(&m).is_err());
    m = manifest("https://user:secret@[::1]/");
    assert!(harness::Bootstrap::parse(&m).is_err());
}
#[test]
fn actual_tls_acquire_two_bookings_renew_marker_release() {
    let mut p = CertificateParams::new(vec![]).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let key = KeyPair::generate().unwrap();
    let ca = p.self_signed(&key).unwrap();
    let leafkey = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["::1".into()])
        .unwrap()
        .signed_by(&leafkey, &ca, &key)
        .unwrap();
    let config = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(leafkey.serialize_der()).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("[::1]:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let mut nonce = None;
        for step in 0..5 {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            s.set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut tls = StreamOwned::new(ServerConnection::new(config.clone()).unwrap(), s);
            let mut request = Vec::new();
            let header_end = loop {
                let mut b = [0; 1024];
                let n = tls.read(&mut b).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&b[..n]);
                if let Some(i) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                    break i + 4;
                }
                assert!(request.len() < 16384);
            };
            let header = String::from_utf8(request[..header_end].to_vec()).unwrap();
            let len: usize = header
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap();
            while request.len() < header_end + len {
                let mut b = [0; 1024];
                let n = tls.read(&mut b).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&b[..n]);
            }
            let body: Value =
                serde_json::from_slice(&request[header_end..header_end + len]).unwrap();
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let (status, response) = match step {
                0 => {
                    assert!(header.starts_with("POST /v0/organisations/org_pub/exchange/versions/ver_test/publisher/lease/acquire "));
                    assert_eq!(body["machineId"], "mac_pub");
                    let n = body["bootNonce"].as_str().unwrap();
                    assert_eq!(n.len(), 22);
                    nonce = Some(n.to_owned());
                    (
                        201,
                        json!({"generation":793,"leaseToken":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","leaseExpiresAtMs":wall+10000,"notBeforeMs":1}),
                    )
                }
                1 | 2 => {
                    let org = if step == 1 { "org_a" } else { "org_b" };
                    assert!(header
                        .starts_with(&format!("POST /v0/organisations/{org}/exchange/bookings ")));
                    assert_eq!(body["durationMs"], 60000);
                    (
                        201,
                        json!({"id":format!("bk:{org}:{}",body["reference"].as_str().unwrap()),"created_at":wall,"expires_at":wall+60000}),
                    )
                }
                3 => {
                    assert!(header.contains("/lease/renew "));
                    assert_eq!(body["generation"], 793);
                    assert_eq!(body["bootNonce"], nonce.as_ref().unwrap().as_str());
                    // Frozen renewal contract: metadata only, no token rotation
                    // and no token echo. The client retains the acquired one.
                    assert_eq!(
                        body["leaseToken"],
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    );
                    (
                        200,
                        json!({"generation":793,"leaseExpiresAtMs":wall+10000,"notBeforeMs":1}),
                    )
                }
                _ => {
                    assert!(header.contains("/lease/release "));
                    assert_eq!(body["generation"], 793);
                    (200, json!({}))
                }
            };
            let bytes = serde_json::to_vec(&response).unwrap();
            write!(tls,"HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",bytes.len()).unwrap();
            tls.write_all(&bytes).unwrap();
            tls.flush().unwrap();
        }
    });
    let transport = VerifiedHttpsTransport::from_trusted_ca_pem(ca.pem().as_bytes()).unwrap();
    let mut client =
        harness::Bootstrap::parse(&manifest(&format!("https://[::1]:{port}/"))).unwrap();
    client.acquire(Arc::new(transport.clone())).unwrap();
    client.book(&transport).unwrap();
    client.renew(&transport).unwrap();
    let dir = tempfile::tempdir().unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let marker = dir.path().join("bookings.json");
    client.write_marker(&marker).unwrap();
    assert!(client.write_marker(&marker).is_err());
    let v = harness::private_json(&marker).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 2);
    assert!(v["bookingA"]
        .as_str()
        .unwrap()
        .starts_with("bk:org_a:engine-"));
    assert!(v["bookingB"]
        .as_str()
        .unwrap()
        .starts_with("bk:org_b:engine-"));
    let mut f = json!({"kind":"exchange-connected-proof-expected-fresh","bookingA":{"id":v["bookingA"]},"bookingB":{"id":v["bookingB"]}});
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    let mut n = 0;
    for (section, keys) in [
        ("shared", ["feed", "version", "publisherMachine"]),
        ("bookingA", ["booking", "takerOrg", "receiverMachine"]),
        ("bookingB", ["booking", "takerOrg", "receiverMachine"]),
    ] {
        for key in keys {
            n += 1;
            f[section][key] = json!(URL_SAFE_NO_PAD.encode([n; 16]));
        }
    }
    f["shared"] = manifest("https://[::1]/")["scenario"]["expectedIdentities"]["shared"].clone();
    f["bookingA"]["booking"] = json!(URL_SAFE_NO_PAD.encode([21; 16]));
    f["bookingA"]["takerOrg"] = json!(URL_SAFE_NO_PAD.encode([22; 16]));
    f["bookingA"]["receiverMachine"] = json!(URL_SAFE_NO_PAD.encode([23; 16]));
    f["bookingB"]["booking"] = json!(URL_SAFE_NO_PAD.encode([24; 16]));
    f["bookingB"]["takerOrg"] = json!(URL_SAFE_NO_PAD.encode([25; 16]));
    f["bookingB"]["receiverMachine"] = json!(URL_SAFE_NO_PAD.encode([26; 16]));
    client.validate_expected(&f).unwrap();
    f["bookingB"]["booking"] = f["bookingA"]["booking"].clone();
    assert!(client.validate_expected(&f).is_err());
    client.release(&transport).unwrap();
    server.join().unwrap();
}

#[test]
fn substituted_booking_ids_fail_before_any_followup() {
    use conspect_media_crypto::authority_http::{
        AuthorityHttpRequest, AuthorityHttpResponse, AuthorityHttpTransport, OrganisationCsk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Bad {
        id: &'static str,
        calls: AtomicUsize,
    }
    impl AuthorityHttpTransport for Bad {
        fn send(
            &self,
            r: AuthorityHttpRequest,
            _: &OrganisationCsk,
        ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
            assert_eq!(
                self.calls.fetch_add(1, Ordering::SeqCst),
                0,
                "no followup request permitted"
            );
            assert!(r.url.ends_with("/bookings"));
            Ok(AuthorityHttpResponse {
                status: 201,
                body: serde_json::to_vec(&json!({"id":self.id})).unwrap(),
            })
        }
    }
    for bad in [
        ".",
        "..",
        "%2e%2e",
        "bk:org_b:other",
        "bk:org_a:wrong-reference",
    ] {
        let t = Bad {
            id: bad,
            calls: AtomicUsize::new(0),
        };
        let mut client = harness::Bootstrap::parse(&manifest("https://[::1]:23456/")).unwrap();
        assert!(client.book(&t).is_err());
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);
    }
}

// T1-T4: entirely in-memory authority and injectable wall/monotonic sleeper.
mod epoch_admission {
    use super::*;
    use conspect_media_crypto::authority_http::{
        AuthorityHttpRequest, AuthorityHttpResponse, AuthorityHttpTransport, GrantHttpResponse,
        OrganisationCsk, PublisherLease,
    };
    use conspect_media_crypto::{Binding, Receiver};
    use ed25519_dalek::{Signer, SigningKey};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use zeroize::Zeroizing;

    fn real_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    struct Clock {
        wall: AtomicU64,
        sleeps: Mutex<Vec<u64>>,
        wake_jump_ms: AtomicU64,
    }
    impl Clock {
        fn new(wall: u64) -> Self {
            Self {
                wall: AtomicU64::new(wall),
                sleeps: Mutex::new(vec![]),
                wake_jump_ms: AtomicU64::new(0),
            }
        }
    }
    impl harness::EpochAdmissionClock for Clock {
        fn wall_ms(&self) -> harness::Result<u64> {
            Ok(self.wall.load(Ordering::SeqCst))
        }
        fn sleep(&self, duration: Duration) {
            let ms = duration.as_millis() as u64;
            self.sleeps.lock().unwrap().push(ms);
            self.wall.fetch_add(
                ms + self.wake_jump_ms.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
        }
    }
    fn valid_test_grant(wall: u64, expiry: u64) -> Vec<u8> {
        // Public test-only key and identities, never fixture authority secrets.
        let signer = SigningKey::from_bytes(&[42; 32]);
        let expected = Binding {
            booking: [1; 16],
            taker: [2; 16],
            feed: [9; 16],
            version: [3; 16],
            engine: [4; 16],
            publisher: [5; 16],
            generation: 0,
            publisher_generation: 1,
        };
        let mut body = [0u8; 200];
        body[..4].copy_from_slice(b"MG02");
        body[4..20].copy_from_slice(b"conspect-exch-v2");
        body[20..36].copy_from_slice(b"conspect-engine2");
        for (offset, id) in [
            (36, expected.booking),
            (52, expected.taker),
            (68, expected.feed),
            (84, expected.version),
            (100, expected.engine),
            (116, expected.publisher),
        ] {
            body[offset..offset + 16].copy_from_slice(&id);
        }
        body[132..140].copy_from_slice(&expected.generation.to_be_bytes());
        body[140..144].copy_from_slice(&expected.publisher_generation.to_be_bytes());
        body[144..152].copy_from_slice(&(wall / 5000).to_be_bytes());
        body[152..160].copy_from_slice(&wall.to_be_bytes());
        body[160..168].copy_from_slice(&expiry.to_be_bytes());
        body[168..200].copy_from_slice(&[7; 32]);
        let mut wire = body.to_vec();
        wire.extend_from_slice(&signer.sign(&body).to_bytes());
        assert!(Receiver::accept(&signer.verifying_key(), expected, &wire, wall, 1000, 20).is_ok());
        wire
    }
    struct Authority {
        clock: Arc<Clock>,
        statuses: Mutex<Vec<u16>>,
        calls: Mutex<Vec<(String, u64, u64)>>,
        lease: Mutex<Option<Arc<PublisherLease>>>,
        close_on_first: bool,
        transport_error: bool,
        wrong_expiry_dto: bool,
        /// Root answer behaviour: see `ROOT_*` below.
        root_mode: AtomicU64,
        /// Arrival wall ms of every current-epoch request.
        root_calls: Mutex<Vec<u64>>,
    }
    /// Answer normally, but hold the answer past the next epoch boundary when
    /// the request arrived in the epoch tail (>= 3500 ms into the epoch).
    const ROOT_HOLD_TAIL: u64 = 0;
    /// Hold only the first answer past the next boundary, wherever it arrived.
    const ROOT_HOLD_FIRST: u64 = 1;
    /// Refuse every root request with 403.
    const ROOT_REFUSE: u64 = 2;
    /// Answer with a rolled-back generation (792 < the lease's 793).
    const ROOT_ROLLBACK: u64 = 3;
    /// Answer for the previous epoch (wrong epoch).
    const ROOT_STALE_EPOCH: u64 = 4;
    /// Added to a mode: hold EVERY answer past the next epoch boundary.
    const ROOT_DELAY_ALL: u64 = 16;
    impl Authority {
        fn new(clock: Arc<Clock>, statuses: Vec<u16>) -> Self {
            Self {
                clock,
                statuses: Mutex::new(statuses),
                calls: Mutex::new(vec![]),
                lease: Mutex::new(None),
                close_on_first: false,
                transport_error: false,
                wrong_expiry_dto: false,
                root_mode: AtomicU64::new(ROOT_HOLD_TAIL),
                root_calls: Mutex::new(vec![]),
            }
        }
    }
    impl AuthorityHttpTransport for Authority {
        fn send(
            &self,
            r: AuthorityHttpRequest,
            _: &OrganisationCsk,
        ) -> conspect_media_crypto::Result<AuthorityHttpResponse> {
            let wall = real_now();
            let (status, body) = if r.url.contains("/lease/acquire") {
                (
                    201,
                    json!({"generation":793,"leaseToken":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","leaseExpiresAtMs":wall+9000,"notBeforeMs":1}),
                )
            } else if r.url.contains("/lease/renew") {
                (
                    200,
                    json!({"generation":793,"leaseExpiresAtMs":wall+9000,"notBeforeMs":1}),
                )
            } else if r.url.contains("/lease/release") {
                (200, json!({}))
            } else if r.url.contains("/current-epoch") {
                let raw_mode = self.root_mode.load(Ordering::SeqCst);
                let delay_all = raw_mode & ROOT_DELAY_ALL != 0;
                let mode = raw_mode & !ROOT_DELAY_ALL;
                let first = {
                    let mut calls = self.root_calls.lock().unwrap();
                    calls.push(wall);
                    calls.len() == 1
                };
                let epoch = if mode == ROOT_STALE_EPOCH {
                    wall / 5000 - 1
                } else {
                    wall / 5000
                };
                let generation = if mode == ROOT_ROLLBACK { 792 } else { 793 };
                let enc = |b| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([b; 16]);
                let body = json!({"versionId":"ver_test","publisherMachineId":"mac_pub","generation":generation,"epoch":epoch,"epochStartMs":epoch*5000,"epochEndMs":epoch*5000+5000,"rootKeyBase64":base64::engine::general_purpose::STANDARD.encode([7u8;32]),"contributorOrgCryptoId":enc(10),"versionCryptoId":enc(3),"publisherMachineCryptoId":enc(5),"feedCryptoId":enc(9)});
                // The answer is built for the arrival epoch and then held past the
                // next boundary: exactly a root answer overtaken in transit.
                if delay_all
                    || (mode == ROOT_HOLD_TAIL && wall % 5000 >= 3500)
                    || (mode == ROOT_HOLD_FIRST && first)
                {
                    std::thread::sleep(Duration::from_millis(5000 - wall % 5000 + 30));
                }
                if mode == ROOT_REFUSE {
                    (403, json!({"error":"refused"}))
                } else {
                    (200, body)
                }
            } else if r.url.ends_with("/bookings") {
                let payload: Value = serde_json::from_slice(&r.body.unwrap()).unwrap();
                let org = if r.url.contains("org_a") {
                    "org_a"
                } else {
                    "org_b"
                };
                let id = format!("bk:{org}:{}", payload["reference"].as_str().unwrap());
                if self.wrong_expiry_dto {
                    (
                        201,
                        json!({"id":id,"created_at":wall,"expiresAt":wall+60000}),
                    )
                } else {
                    (
                        201,
                        json!({"id":id,"created_at":wall,"expires_at":wall+60000}),
                    )
                }
            } else {
                panic!("unexpected authority route")
            };
            Ok(AuthorityHttpResponse {
                status,
                body: serde_json::to_vec(&body).unwrap(),
            })
        }
        fn send_grant(
            &self,
            r: AuthorityHttpRequest,
            _: &OrganisationCsk,
        ) -> conspect_media_crypto::Result<GrantHttpResponse> {
            if self.transport_error {
                return Err(conspect_media_crypto::Error::Denied);
            }
            let body: Value = serde_json::from_slice(&r.body.unwrap()).unwrap();
            let wall = self.clock.wall.load(Ordering::SeqCst);
            let requested = body["requestedExpiryMs"].as_u64().unwrap();
            assert!(requested < ((wall / 5000 + 1) * 5000));
            assert!(requested > wall + 1000);
            self.calls.lock().unwrap().push((r.url, wall, requested));
            let status = {
                let mut statuses = self.statuses.lock().unwrap();
                if statuses.is_empty() {
                    200
                } else {
                    statuses.remove(0)
                }
            };
            if self.close_on_first && self.calls.lock().unwrap().len() == 1 {
                self.lease
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .release()
                    .unwrap();
            }
            let bytes = if status == 200 {
                valid_test_grant(wall, requested)
            } else {
                vec![]
            };
            Ok(GrantHttpResponse {
                status,
                body: Zeroizing::new(bytes),
                content_type: "application/octet-stream".into(),
                cache_control: "no-store, private".into(),
                content_length: Some(264),
            })
        }
    }
    fn setup(clock: Arc<Clock>, authority: Arc<Authority>) -> harness::Bootstrap {
        let mut b = harness::Bootstrap::parse(&manifest("https://[::1]:23456/"))
            .unwrap()
            .with_admission_clock(clock);
        b.acquire(authority.clone()).unwrap();
        b.book(authority.as_ref()).unwrap();
        *authority.lease.lock().unwrap() = Some(b.publisher_lease().unwrap());
        b
    }
    #[test]
    fn tail_404_retries_once_after_boundary_then_mints_a_and_b_without_installing_grants() {
        let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 3200));
        let authority = Arc::new(Authority::new(clock.clone(), vec![404, 200, 200]));
        let mut b = setup(clock.clone(), authority.clone());
        b.mint_mapping(authority.as_ref()).unwrap();
        let calls = authority.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].0.contains("org_a"));
        assert_eq!(calls[0].0, calls[1].0);
        assert!(calls[1].1 / 5000 > calls[0].1 / 5000);
        assert!(calls[2].0.contains("org_b"));
        assert_eq!(clock.sleeps.lock().unwrap().len(), 1);
        drop(calls);
        // Mapping bytes were dropped. Neither a malformed fragment nor no fragment
        // can enable runtime grant identity use (no third/fourth request).
        assert!(b.grant(authority.as_ref(), 0).is_err());
        assert!(b.validate_expected(&json!({})).is_err());
        assert!(b.grant(authority.as_ref(), 0).is_err());
        assert_eq!(authority.calls.lock().unwrap().len(), 3);
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("mapping.json");
        b.write_marker(&marker).unwrap();
        let ids: Value = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        let enc = |n| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([n; 16]);
        let expected = json!({"kind":"exchange-connected-proof-expected-fresh","shared":manifest("https://[::1]/")["scenario"]["expectedIdentities"]["shared"],
            "bookingA":{"id":ids["bookingA"],"booking":enc(21),"takerOrg":enc(22),"receiverMachine":enc(23)},
            "bookingB":{"id":ids["bookingB"],"booking":enc(24),"takerOrg":enc(25),"receiverMachine":enc(26)}});
        b.validate_expected(&expected).unwrap();
        assert_eq!(b.grant(authority.as_ref(), 0).unwrap().len(), 264);
        assert_eq!(authority.calls.lock().unwrap().len(), 4);
    }
    #[test]
    fn prewait_never_sends_a_grant_in_tail() {
        let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 4000));
        let authority = Arc::new(Authority::new(clock.clone(), vec![200, 200]));
        let mut b = setup(clock.clone(), authority.clone());
        b.mint_mapping(authority.as_ref()).unwrap();
        let calls = authority.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].1 % 5000 >= 250 && calls[0].1 % 5000 < 3500);
        assert_eq!(clock.sleeps.lock().unwrap().len(), 1);
    }
    #[test]
    fn foreign_booking_twice_404_is_exactly_two_calls() {
        let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 2800));
        let authority = Arc::new(Authority::new(clock.clone(), vec![404, 404]));
        let mut b = setup(clock.clone(), authority.clone());
        assert!(b.mint_mapping(authority.as_ref()).is_err());
        assert_eq!(authority.calls.lock().unwrap().len(), 2);
        assert_eq!(clock.sleeps.lock().unwrap().len(), 1);
    }
    #[test]
    fn rejects_camel_case_booking_expiry_not_on_real_201_dto() {
        let clock = Arc::new(Clock::new(real_now()));
        let mut transport = Authority::new(clock, vec![]);
        transport.wrong_expiry_dto = true;
        let authority = Arc::new(transport);
        let mut b = harness::Bootstrap::parse(&manifest("https://[::1]:23456/")).unwrap();
        b.acquire(authority.clone()).unwrap();
        assert_eq!(b.book(authority.as_ref()), Err("booking expiry missing"));
        assert!(authority.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn booking_expired_during_prewait_denies_without_grant_request() {
        let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 4000));
        let authority = Arc::new(Authority::new(clock.clone(), vec![200]));
        let mut b = setup(clock.clone(), authority.clone());
        clock.wake_jump_ms.store(120_000, Ordering::SeqCst);
        assert!(b.mint_mapping(authority.as_ref()).is_err());
        assert!(authority.calls.lock().unwrap().is_empty());
        assert_eq!(clock.sleeps.lock().unwrap().len(), 1);
    }
    #[test]
    fn expired_booking_denies_before_any_grant_request() {
        let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 2800));
        let authority = Arc::new(Authority::new(clock.clone(), vec![200]));
        let mut b = setup(clock.clone(), authority.clone());
        clock.wall.fetch_add(120_000, Ordering::SeqCst);
        assert!(b.mint_mapping(authority.as_ref()).is_err());
        assert!(authority.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn lease_revoked_after_404_and_transport_and_non404_never_retry() {
        for mode in 0..3 {
            let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 2800));
            let mut transport =
                Authority::new(clock.clone(), vec![if mode == 1 { 403 } else { 404 }]);
            transport.close_on_first = mode == 0;
            transport.transport_error = mode == 2;
            let authority = Arc::new(transport);
            let mut b = setup(clock.clone(), authority.clone());
            assert!(b.mint_mapping(authority.as_ref()).is_err());
            assert_eq!(
                authority.calls.lock().unwrap().len(),
                if mode == 2 { 0 } else { 1 }
            );
            assert!(clock.sleeps.lock().unwrap().is_empty());
        }
    }

    /// Wait on the real clock until the epoch phase is inside `[from, to)`.
    fn wait_for_phase(from: u64, to: u64) {
        loop {
            let phase = real_now() % 5000;
            if (from..to).contains(&phase) {
                return;
            }
            let wait = (from + 5000 - phase) % 5000;
            std::thread::sleep(Duration::from_millis(wait.max(1)));
        }
    }
    /// D3: the startup root fetch must be admitted out of the epoch tail like a
    /// grant. Started 4.6 s into an epoch, a root request sent at once is
    /// answered in the next epoch and rejected as expired: before the fix this
    /// ended the run with "canonical root rejected".
    #[test]
    fn startup_root_fetch_is_admitted_out_of_the_epoch_tail() {
        // The grant admission clock is parked mid-epoch so only the root
        // fetch's own admission is under test.
        let clock = Arc::new(Clock::new(real_now() / 5000 * 5000 + 1000));
        let authority = Arc::new(Authority::new(clock.clone(), vec![]));
        let mut b = setup(clock, authority.clone());
        wait_for_phase(4600, 4700);
        b.renew(authority.as_ref()).unwrap();
        let result = b.mint_mapping(authority.as_ref());
        let calls = authority.root_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0] % 5000 >= 250 && calls[0] % 5000 < 3500,
            "root requested in the epoch tail at phase {} ms",
            calls[0] % 5000
        );
        assert_eq!(result, Ok(()));
    }
    /// D3: an answer overtaken by the boundary in transit is reported as the one
    /// retryable root error, not as a refusal, and the next request succeeds.
    #[test]
    fn root_answer_overtaken_by_the_boundary_is_retryable_once() {
        let clock = Arc::new(Clock::new(real_now()));
        let authority = Arc::new(Authority::new(clock.clone(), vec![]));
        authority.root_mode.store(ROOT_HOLD_FIRST, Ordering::SeqCst);
        let mut b = setup(clock, authority.clone());
        wait_for_phase(500, 1000);
        b.renew(authority.as_ref()).unwrap();
        assert_eq!(
            b.root(authority.as_ref()),
            Err("canonical root expired in transit")
        );
        assert_eq!(
            harness::ROOT_EXPIRED_IN_TRANSIT,
            "canonical root expired in transit"
        );
        b.renew(authority.as_ref()).unwrap();
        assert_eq!(b.root(authority.as_ref()), Ok(Value::Null));
        assert_eq!(authority.root_calls.lock().unwrap().len(), 2);
    }
    /// A refusal, a generation rollback and an answer for the wrong epoch are
    /// fatal root rejections, never the retryable in-transit expiry, whatever
    /// the epoch phase.
    #[test]
    fn root_refusal_rollback_and_wrong_epoch_are_never_retryable() {
        for mode in [ROOT_REFUSE, ROOT_ROLLBACK, ROOT_STALE_EPOCH] {
            let clock = Arc::new(Clock::new(real_now()));
            let authority = Arc::new(Authority::new(clock.clone(), vec![]));
            authority.root_mode.store(mode, Ordering::SeqCst);
            let mut b = setup(clock, authority.clone());
            wait_for_phase(500, 1000);
            b.renew(authority.as_ref()).unwrap();
            assert_eq!(
                b.root(authority.as_ref()),
                Err("canonical root rejected"),
                "mode {mode}"
            );
            assert_eq!(authority.root_calls.lock().unwrap().len(), 1);
        }
    }
    /// The authority cannot turn a refusal or a rollback into the retryable
    /// in-transit class by holding its answer past the boundary: the provider
    /// classifies the status and the generation before any epoch check.
    #[test]
    fn delayed_refusal_and_rollback_stay_fatal() {
        for mode in [ROOT_REFUSE, ROOT_ROLLBACK] {
            let clock = Arc::new(Clock::new(real_now()));
            let authority = Arc::new(Authority::new(clock.clone(), vec![]));
            authority
                .root_mode
                .store(mode | ROOT_DELAY_ALL, Ordering::SeqCst);
            let mut b = setup(clock, authority.clone());
            wait_for_phase(500, 1000);
            b.renew(authority.as_ref()).unwrap();
            assert_eq!(
                b.root(authority.as_ref()),
                Err("canonical root rejected"),
                "mode {mode}"
            );
            assert_eq!(authority.root_calls.lock().unwrap().len(), 1);
        }
    }
    /// A wrong-epoch answer held past the boundary is indistinguishable at the
    /// provider from an answer overtaken in transit (both are an epoch mismatch
    /// after the boundary), so each call reports the retryable class; the
    /// runtime's single-retry budget bounds the cost at two requests.
    #[test]
    fn delayed_wrong_epoch_root_is_classified_in_transit() {
        let clock = Arc::new(Clock::new(real_now()));
        let authority = Arc::new(Authority::new(clock.clone(), vec![]));
        authority
            .root_mode
            .store(ROOT_STALE_EPOCH | ROOT_DELAY_ALL, Ordering::SeqCst);
        let mut b = setup(clock, authority.clone());
        wait_for_phase(500, 1000);
        b.renew(authority.as_ref()).unwrap();
        assert_eq!(
            b.root(authority.as_ref()),
            Err("canonical root expired in transit")
        );
        assert_eq!(authority.root_calls.lock().unwrap().len(), 1);
    }
    /// A lease that closed while the root request was in flight (a withdrawal,
    /// a revocation) is fatal even when the epoch also advanced.
    #[test]
    fn root_expired_after_the_lease_closed_is_not_retryable() {
        let clock = Arc::new(Clock::new(real_now()));
        let authority = Arc::new(Authority::new(clock.clone(), vec![]));
        authority.root_mode.store(ROOT_HOLD_FIRST, Ordering::SeqCst);
        let mut b = setup(clock, authority.clone());
        wait_for_phase(500, 1000);
        b.renew(authority.as_ref()).unwrap();
        let lease = b.publisher_lease().unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1500));
            lease.release().unwrap();
        });
        let result = b.root(authority.as_ref());
        closer.join().unwrap();
        assert_eq!(result, Err("canonical root rejected"));
    }
}
