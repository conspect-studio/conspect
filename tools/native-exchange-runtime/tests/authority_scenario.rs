//! Local TLS authority drives the entire native command, not a parser-only probe.
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use ed25519_dalek::{Signer, SigningKey};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use rustls::{pki_types::PrivatePkcs8KeyDer, ServerConfig, ServerConnection, StreamOwned};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
#[test]
fn release_command_invokes_tls_authority_media_receipts_and_cutoff() {
    scenario("release", false, false, "");
}
#[test]
fn withdraw_command_cuts_off_both_takers() {
    scenario("withdraw", false, false, "");
}
#[test]
fn exhaustion_command_uses_decoded_receipts() {
    scenario("exhaustion", false, false, "");
}
#[test]
fn slow_native_decoder_keeps_tls_lease_and_bookings_alive() {
    scenario("release", true, false, "");
}
#[test]
fn lease_denial_flushes_while_native_decoder_stalls() {
    scenario("release", true, true, "");
}
#[test]
fn raw_root_identity_mismatch_fails_closed() {
    scenario("raw-mismatch", false, false, "");
}
#[test]
fn missing_authority_window_fails_closed() {
    scenario("missing-window", false, false, "");
}
#[test]
fn late_authority_headers_fail_closed() {
    scenario("late-root", false, false, "");
}
/// Booking A's paid window for the credit-cutoff fixtures: a fixture value (the
/// web seed sizes A's credit as ceil(rate x duration / 60000)), long enough that
/// the fixed costs (fixture encode, TLS, handoff, decode start) cannot consume it
/// on a loaded runner. The mock and the assertions derive everything from it.
const CUTOFF_WINDOW_MS: u64 = 30_000;

#[test]
fn credit_cutoff_uses_early_credit_refusal_and_paid_boundary() {
    scenario("credit-cutoff", false, false, "");
}
/// Deterministic reproduction of #248: B's grant answers are held 1.5-2.5 s for
/// requests that arrive within 1 s of an epoch boundary (and the first 200 ms
/// after it). B must keep decoding on all six tracks across at least two
/// boundaries; a late grant is not a denial.
#[test]
fn credit_cutoff_late_b_grants_do_not_stop_b_decoding() {
    scenario("credit-cutoff", false, false, "late-b-grants");
}
/// Deliberate negative: the expected-fresh fragment arrives after A's paid window
/// is (nearly) over. The command must refuse with the static reason.
#[test]
fn credit_cutoff_fragment_past_a_window_fails_with_static_reason() {
    scenario("credit-cutoff", false, false, "late-fragment");
}
/// Deterministic reproduction of the CI `FFmpeg command failed` mode: B's epoch
/// grant is unobtainable for the whole epoch that precedes A's expiry, so every
/// object B parked in that epoch is dropped as stale. A taker with no accepted
/// bytes for a track has nothing to decode; that is not a decode failure and
/// must not end the run before A's paid boundary is probed (404) at expiry.
#[test]
fn credit_cutoff_stale_b_epoch_still_probes_the_paid_boundary() {
    scenario("credit-cutoff", false, false, "late-b-epoch");
}
#[test]
fn credit_cutoff_late_mapping_answer_still_probes_the_paid_boundary() {
    scenario("credit-cutoff", false, false, "late-mapping-grant");
}
#[test]
fn credit_cutoff_mapping_refusal_is_not_retried() {
    scenario("credit-cutoff", false, false, "mapping-refusal");
}
/// D3: the authority's takeover fence (`notBeforeMs`) ends 350 ms before an
/// epoch boundary, so the client's startup root fetch is due in the epoch tail.
/// A root request sent there is answered for its arrival epoch after the
/// boundary and is rejected as expired; before the fix that ended the whole run
/// ("canonical root rejected") before A's paid boundary was ever probed.
#[test]
fn credit_cutoff_startup_root_in_epoch_tail_still_probes_the_paid_boundary() {
    scenario("credit-cutoff", false, false, "root-in-epoch-tail");
}
fn scenario(scenario: &'static str, slow: bool, deny_renew: bool, variant: &'static str) {
    // Admit one six-stream FFmpeg/TLS fixture at a time. These wall-clock
    // integration scenarios have real five-second grants; eight concurrent
    // encoders/decoders can miss their epoch under host contention. This bounds
    // test resource demand without changing any authority deadline or assertion.
    static ADMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _admission = ADMISSION.lock().unwrap_or_else(|p| p.into_inner());
    let negative = matches!(scenario, "raw-mismatch" | "missing-window" | "late-root");
    let late_fragment = variant == "late-fragment";
    // The late-fragment fixture uses a short paid window and holds the fragment
    // past it; the others use the long window.
    let window_ms: u64 = if late_fragment {
        4000
    } else {
        CUTOFF_WINDOW_MS
    };
    let mut p = CertificateParams::new(vec![]).unwrap();
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca_key = KeyPair::generate().unwrap();
    let ca = p.self_signed(&ca_key).unwrap();
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["::1".into()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
        .unwrap();
    let config = Arc::new(
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![leaf.der().clone()],
                PrivatePkcs8KeyDer::from(leaf_key.serialize_der()).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("[::1]:0").unwrap();
    let origin = format!("https://[::1]:{}", listener.local_addr().unwrap().port());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let marker = dir.path().join("marker");
    let expected_path = dir.path().join("expected");
    let expected_server = expected_path.clone();
    let signer = SigningKey::from_bytes(&[42; 32]);
    let public = URL_SAFE_NO_PAD.encode(signer.verifying_key().as_bytes());
    let server = std::thread::spawn(move || {
        let mut books = Vec::new();
        let mut expiry = [0u64; 2];
        let mut boundary_denials = 0;
        let mut denied = false;
        let mut receipts = 0;
        let mut charged_totals = [0u64; 2];
        let mut roots = 0;
        let mut renewals = Vec::new();
        let mut extensions = [Vec::new(), Vec::new()];
        let mut deferred: Vec<std::thread::JoinHandle<()>> = Vec::new();
        let mut delayed_grants = 0u32;
        let mut mapping_grants = 0u32;
        // root-in-epoch-tail: the lease's notBeforeMs (wall) and the epoch phase
        // at which the first current-epoch request arrived.
        let mut not_before_ms = 1u64;
        let mut first_root_phase: Option<u64> = None;
        let mut held_roots = 0u32;
        while !stopped.load(Ordering::SeqCst) {
            let (s, _) = match listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("accept {e}"),
            };
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut tls = StreamOwned::new(ServerConnection::new(config.clone()).unwrap(), s);
            let mut request = Vec::new();
            let end = loop {
                let mut b = [0; 4096];
                let n = tls.read(&mut b).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&b[..n]);
                if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let header = String::from_utf8(request[..end].to_vec()).unwrap();
            let line = header.lines().next().unwrap();
            let len: usize = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while request.len() < end + len {
                let mut b = [0; 4096];
                let n = tls.read(&mut b).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&b[..n]);
            }
            let body: Value = if len == 0 {
                json!({})
            } else {
                serde_json::from_slice(&request[end..end + len]).unwrap()
            };
            let path = line.split_whitespace().nth(1).unwrap();
            let mut status = 200;
            let mut binary = false;
            let response = if path.ends_with("/lease/acquire") {
                status = 201;
                if variant == "root-in-epoch-tail" {
                    // The client may seal (and so fetch a root) from notBeforeMs
                    // plus its 1000 ms clock uncertainty: place that instant at
                    // epoch phase 4650, at least 2 s from now.
                    let target = (now() + 2000) / 5000 * 5000 + 4650;
                    let target = if target < now() + 2000 {
                        target + 5000
                    } else {
                        target
                    };
                    not_before_ms = target - 1000;
                }
                // The token is minted once, on acquire, and never appears on a
                // renewal answer.
                json!({"generation":793,"leaseToken":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","leaseExpiresAtMs":now()+10000,"notBeforeMs":not_before_ms})
            } else if path.ends_with("/lease/renew") {
                renewals.push(std::time::Instant::now());
                if deny_renew && renewals.len() >= 8 {
                    status = 403;
                }
                // Contract: a global withdrawal also revokes the lease route.
                if scenario == "withdraw" && denied {
                    status = 404;
                }
                // Frozen renewal contract: metadata only, the token is neither
                // rotated nor echoed, so the client must retain the acquired one.
                assert!(
                    body["leaseToken"].as_str().is_some(),
                    "a renewal must present the retained lease token"
                );
                json!({"generation":793,"leaseExpiresAtMs":now()+10000,"notBeforeMs":not_before_ms})
            } else if path.ends_with("/lease/release") {
                if scenario == "withdraw" && denied {
                    status = 404;
                }
                json!({})
            } else if path.ends_with("/exchange/bookings") {
                status = 201;
                let i = books.len();
                let org = if i == 0 { "org_a" } else { "org_b" };
                let id = format!("bk:{org}:{}", body["reference"].as_str().unwrap());
                assert_eq!(
                    body["durationMs"],
                    if scenario == "credit-cutoff" && i == 0 {
                        window_ms
                    } else {
                        60000
                    }
                );
                let created = now();
                expiry[i] = created + body["durationMs"].as_u64().unwrap();
                books.push(id.clone());
                if books.len() == 2 {
                    let v = json!({"kind":"exchange-connected-proof-expected-fresh","shared":{"contributorOrg":URL_SAFE_NO_PAD.encode([10;16]),"publisherInstance":URL_SAFE_NO_PAD.encode([5;16]),"feed":URL_SAFE_NO_PAD.encode([9;16]),"version":URL_SAFE_NO_PAD.encode([3;16]),"publisherMachine":URL_SAFE_NO_PAD.encode([5;16])},"bookingA":{"id":books[0],"booking":URL_SAFE_NO_PAD.encode([1;16]),"takerOrg":URL_SAFE_NO_PAD.encode([2;16]),"receiverMachine":URL_SAFE_NO_PAD.encode([4;16])},"bookingB":{"id":books[1],"booking":URL_SAFE_NO_PAD.encode([6;16]),"takerOrg":URL_SAFE_NO_PAD.encode([7;16]),"receiverMachine":URL_SAFE_NO_PAD.encode([8;16])}});
                    let write = {
                        let (path, bytes) =
                            (expected_server.clone(), serde_json::to_vec(&v).unwrap());
                        move || {
                            std::fs::write(&path, bytes).unwrap();
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                                .unwrap();
                        }
                    };
                    if late_fragment {
                        // Hold the fragment until A's paid window has <2 s left.
                        let wait = expiry[0].saturating_sub(2000).saturating_sub(now()) + 100;
                        deferred.push(std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(wait));
                            write();
                        }));
                    } else {
                        write();
                    }
                }
                json!({"id":id,"created_at":created,"expires_at":expiry[i]})
            } else if path.contains("/current-epoch?") {
                // No seal may follow a withdrawal: the engine must stop at once.
                assert!(
                    !(scenario == "withdraw" && denied),
                    "root fetched after the withdrawal (sealing continued)"
                );
                roots += 1;
                let phase = now() % 5000;
                first_root_phase.get_or_insert(phase);
                if variant == "root-in-epoch-tail" && roots == 1 && phase >= 4000 {
                    // Built below for the arrival epoch, answered 30 ms after the
                    // boundary (well inside the 1 s transport timeout).
                    held_roots += 1;
                    eprintln!("mock: first root request at epoch phase {phase} ms; answer crosses the boundary");
                }
                let mut dto = json!({"versionId":"ver_test","publisherMachineId":"mac_pub","generation":793,"epoch":now()/5000,"epochStartMs":now()/5000*5000,"epochEndMs":(now()/5000+1)*5000,"feedCryptoId":URL_SAFE_NO_PAD.encode([9;16]),"versionCryptoId":URL_SAFE_NO_PAD.encode([3;16]),"publisherMachineCryptoId":URL_SAFE_NO_PAD.encode([5;16]),"contributorOrgCryptoId":URL_SAFE_NO_PAD.encode([10;16]),"rootKeyBase64":STANDARD.encode([11;32])});
                if scenario == "raw-mismatch" {
                    dto["feedCryptoId"] = json!(URL_SAFE_NO_PAD.encode([99; 16]));
                }
                if scenario == "missing-window" {
                    dto.as_object_mut().unwrap().remove("epochEndMs");
                }
                if held_roots == 1 && roots == 1 {
                    std::thread::sleep(std::time::Duration::from_millis(5000 - phase + 30));
                }
                if scenario == "late-root" {
                    std::thread::sleep(std::time::Duration::from_millis(5000 - now() % 5000 + 20));
                }
                dto
            } else if path.ends_with("/grants") {
                if books.len() == 2 && path.contains("org_a/") {
                    mapping_grants += 1;
                }
                if variant == "mapping-refusal" && mapping_grants == 1 {
                    status = 403;
                    json!({"error":"refused"})
                } else if scenario == "credit-cutoff"
                    && path.contains("org_a/")
                    && now() >= expiry[0]
                {
                    boundary_denials += 1;
                    status = 404;
                    json!({"type":"about:blank","title":"Not Found","status":404,"detail":"booking expired"})
                } else if denied && (path.contains("org_a/") || scenario == "withdraw") {
                    status = 403;
                    json!({"error":"released"})
                } else {
                    binary = true;
                    let i = usize::from(path.contains("org_b/"));
                    let mut g = Vec::new();
                    g.extend(b"MG02");
                    g.extend(b"conspect-exch-v2");
                    g.extend(b"conspect-engine2");
                    for id in [
                        if i == 0 { 1 } else { 6 },
                        if i == 0 { 2 } else { 7 },
                        9,
                        3,
                        if i == 0 { 4 } else { 8 },
                        5,
                    ] {
                        g.extend([id; 16]);
                    }
                    g.extend(0u64.to_be_bytes());
                    g.extend(793u32.to_be_bytes());
                    g.extend((now() / 5000).to_be_bytes());
                    g.extend((now() / 5000 * 5000).to_be_bytes());
                    let grant_end = if scenario == "credit-cutoff" {
                        ((now() / 5000 + 1) * 5000).min(expiry[i])
                    } else {
                        (now() / 5000 + 1) * 5000
                    };
                    g.extend(grant_end.to_be_bytes());
                    g.extend([11; 32]);
                    g.extend(signer.sign(&g).to_bytes());
                    json!(g)
                }
            } else if path.ends_with("/extend") {
                status = 201;
                let i = usize::from(path.contains("org_b/"));
                extensions[i].push(std::time::Instant::now());
                if scenario == "credit-cutoff" && i == 0 {
                    assert!(now() < expiry[0]);
                    assert_eq!(extensions[0].len(), 1);
                    assert_eq!(body["extraDurationMs"], 5000);
                    status = 409;
                    json!({"type":"about:blank","title":"Conflict","status":409,"detail":"The prepaid exchange balance cannot cover the additional reservation; the expiry is unchanged."})
                } else {
                    assert_eq!(body["extraDurationMs"], 20000);
                    expiry[i] += 20000;
                    json!({"id":books[i],"expires_at":expiry[i]})
                }
            } else if path.ends_with("/receipts") {
                assert_eq!(body["quality"]["width"], 64);
                assert_eq!(body["quality"]["height"], 64);
                assert_eq!(body["quality"]["fps"], 10.0);
                for unknown in ["bitrateBps", "jitterMs", "queueAgeMs", "backpressure"] {
                    assert!(body["quality"].get(unknown).is_none());
                }
                assert_eq!(
                    URL_SAFE_NO_PAD
                        .decode(body["receiptId"].as_str().unwrap())
                        .unwrap()
                        .len(),
                    16
                );
                assert!(
                    body["mediaEndMs"].as_u64().unwrap() <= body["observedAt"].as_u64().unwrap()
                );
                if scenario == "credit-cutoff" && path.contains("org_a/") {
                    assert!(body["mediaEndMs"].as_u64().unwrap() <= expiry[0]);
                }
                receipts += 1;
                if scenario == "exhaustion" && path.contains("org_a/") {
                    denied = true;
                }
                let i = usize::from(path.contains("org_b/"));
                charged_totals[i] += 1;
                json!({"sequence":body["sequence"],"chargedMicro":1,"chargedMicroTotal":charged_totals[i],"reservedMicro":10000,"stopReason":if scenario=="exhaustion"&&path.contains("org_a/"){Some("reservation_exhausted")}else{None}})
            } else if path.ends_with("/withdraw") {
                denied = true;
                json!({})
            } else if path.ends_with("/release") {
                status = 201;
                denied = true;
                json!({})
            } else {
                panic!("unexpected path {path}")
            };
            let bytes = if binary {
                response
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_u64().unwrap() as u8)
                    .collect::<Vec<_>>()
            } else {
                serde_json::to_vec(&response).unwrap()
            };
            let typ = if binary {
                "application/octet-stream"
            } else {
                "application/json"
            };
            let mut hold_ms = 0u64;
            if variant == "late-mapping-grant"
                && mapping_grants == 1
                && path.ends_with("/grants")
                && path.contains("org_a/")
            {
                hold_ms = 1500;
                delayed_grants += 1;
            }
            if variant == "late-b-grants" && path.ends_with("/grants") && path.contains("org_b/") {
                let phase = now() % 5000;
                if !(200..4000).contains(&phase) {
                    delayed_grants += 1;
                    hold_ms = 1500 + 500 * u64::from(delayed_grants % 3);
                    eprintln!(
                        "mock: holding B grant answer {hold_ms} ms (request at epoch phase {phase} ms)"
                    );
                }
            }
            if variant == "late-b-epoch"
                && path.ends_with("/grants")
                && path.contains("org_b/")
                && books.len() == 2
                // The epoch immediately before the one holding A's expiry: B's
                // grant is unobtainable for that entire epoch (every request is
                // answered after the epoch ends), so everything it parks there
                // ages past the replay window and is dropped as stale. The next
                // epoch's grants are answered normally, so B decodes again.
                && now() >= expiry[0].saturating_sub(window_ms).saturating_add(5000)
                && now() < expiry[0].saturating_add(5000)
            {
                delayed_grants += 1;
                hold_ms = 5000;
                eprintln!("mock: holding B grant answer {hold_ms} ms for the pre-expiry epoch");
            }
            let write_answer = move || {
                let mut tls = tls;
                if hold_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(hold_ms));
                }
                let sent = (|| -> std::io::Result<()> {
                    write!(tls,"HTTP/1.1 {status} OK\r\ncontent-type: {typ}\r\ncontent-length: {}\r\ncache-control: no-store, private\r\nconnection: close\r\n\r\n",bytes.len())?;
                    tls.write_all(&bytes)?;
                    tls.flush()
                })();
                if let Err(error) = sent {
                    // A deliberately late response can outlive the client's deadline.
                    // All negative-path receipt and command assertions still apply.
                    assert!(
                        (scenario == "late-root" || hold_ms > 0)
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                            )
                    );
                }
            };
            if hold_ms > 0 {
                deferred.push(std::thread::spawn(write_answer));
            } else {
                write_answer();
            }
        }
        for handle in deferred {
            handle.join().unwrap();
        }
        if negative {
            assert_eq!(
                receipts, 0,
                "rejected authority must not emit decoded receipts"
            );
        } else if deny_renew {
            assert!(renewals.len() >= 3);
        } else if slow {
            assert!(renewals.len() >= 6, "renewals during native decoder stall");
            assert!(renewals
                .windows(2)
                .all(|w| w[1].duration_since(w[0]) <= std::time::Duration::from_secs(5)));
            for e in extensions {
                assert!(!e.is_empty());
                assert!(e
                    .windows(2)
                    .all(|w| w[1].duration_since(w[0]) <= std::time::Duration::from_secs(30)));
            }
        } else if variant == "mapping-refusal" {
            assert_eq!(mapping_grants, 1, "an answered 403 must not be retried");
        } else if variant == "late-mapping-grant" {
            assert_eq!(delayed_grants, 1);
            assert!(mapping_grants >= 2, "transport timeout was not retried");
            assert!(boundary_denials > 0, "A paid boundary not probed");
            assert!(receipts > 0);
        } else if variant == "root-in-epoch-tail" {
            // The fence really ended in the tail, and the client still fetched its
            // first root outside the tail, then reached A's paid boundary.
            assert!(not_before_ms > 1 && (not_before_ms + 1000) % 5000 == 4650);
            let phase = first_root_phase.expect("no root fetched");
            assert!(
                (250..3500).contains(&phase),
                "first root request in the epoch tail at phase {phase} ms"
            );
            assert!(boundary_denials > 0, "A paid boundary not probed");
            assert_eq!(extensions[0].len(), 1);
            assert!(!extensions[1].is_empty(), "B must extend with 201");
            assert!(receipts > 0);
        } else if variant == "late-b-grants" {
            assert!(delayed_grants > 0, "the mock never delayed a B grant");
            assert!(boundary_denials > 0);
            assert_eq!(extensions[0].len(), 1);
            assert!(!extensions[1].is_empty(), "B must extend with 201");
            assert!(receipts > 0);
        } else if variant == "late-b-epoch" {
            assert!(delayed_grants > 0, "the mock never delayed a B grant");
            assert!(
                boundary_denials > 0,
                "A's paid boundary was never probed after the stale B epoch"
            );
            assert_eq!(extensions[0].len(), 1);
            assert!(!extensions[1].is_empty(), "B must extend with 201");
            assert!(receipts > 0);
        } else if late_fragment {
            // No media may be produced for A's elapsed window: no receipts.
            assert_eq!(receipts, 0, "no media may follow a late fragment");
        } else if scenario == "credit-cutoff" {
            assert!(boundary_denials > 0);
            assert_eq!(extensions[0].len(), 1);
            assert!(!extensions[1].is_empty(), "B must extend with 201");
            assert!(receipts > 0);
        } else {
            assert!(denied);
            assert!(receipts > 0);
        }
        assert!(roots > 0);
    });
    let mut manifest = json!({"kind":"exchange-connected-proof-harness","baseUrl":origin,"basePath":"/v0","caPublic":ca.pem(),"mg02Verification":{"jwk":{"x":public}},"scenario":{"expectedIdentities":{"shared":{"feed":URL_SAFE_NO_PAD.encode([9;16]),"version":URL_SAFE_NO_PAD.encode([3;16]),"publisherMachine":URL_SAFE_NO_PAD.encode([5;16]),"contributorOrg":URL_SAFE_NO_PAD.encode([10;16]),"publisherInstance":URL_SAFE_NO_PAD.encode([5;16])}},"versionId":"ver_test","storyId":"story_test","publisherLease":{"bootstrapOnly":true,"adoptable":false}},"credentials":{"contributor":{"orgId":"org_pub","machineId":"mac_pub","token":"synthetic-pub"},"takerA":{"orgId":"org_a","machineId":"mac_a","token":"synthetic-a"},"takerB":{"orgId":"org_b","machineId":"mac_b","token":"synthetic-b"}}});
    if scenario == "credit-cutoff" {
        manifest["scenario"]["fixtureProfile"] =
            json!({"name":"funded-expiry","bookingDurationMs":window_ms});
    }
    let manifest_path = dir.path().join("manifest");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut command =
        std::process::Command::new(env!("CARGO_BIN_EXE_conspect-native-exchange-runtime"));
    if slow {
        let ffmpeg = std::process::Command::new("which")
            .arg("ffmpeg")
            .output()
            .unwrap();
        assert!(ffmpeg.status.success());
        let ffmpeg = String::from_utf8(ffmpeg.stdout).unwrap();
        let wrapper = dir.path().join("ffmpeg");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\ncase \" $* \" in *showinfo*) sleep 35;; esac\nexec {} \"$@\"\n",
                ffmpeg.trim()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        command.env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        );
    }
    let output = command
        .args([
            "--bootstrap",
            manifest_path.to_str().unwrap(),
            "--marker",
            marker.to_str().unwrap(),
            "--expected",
            expected_path.to_str().unwrap(),
            "--scenario",
            if negative { "release" } else { scenario },
        ])
        .output()
        .unwrap();
    stop.store(true, Ordering::SeqCst);
    let server_result = server.join();
    eprintln!(
        "command status={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    server_result.unwrap();
    if variant == "mapping-refusal" {
        assert!(!output.status.success(), "refused mapping must fail closed");
        assert!(String::from_utf8_lossy(&output.stderr).contains("mapping grant rejected"));
        return;
    }
    if negative {
        assert!(
            !output.status.success(),
            "invalid authority must fail closed"
        );
        return;
    }
    if deny_renew {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let flush = stderr
            .lines()
            .find_map(|l| {
                l.strip_prefix("control fence denied; native receiver queues flushed at ")
            })
            .unwrap();
        let flush: u64 = flush.parse().unwrap();
        assert!(
            now() - flush >= 20000,
            "flush must precede decoder completion"
        );
    }
    if slow {
        assert!(
            !output.status.success(),
            "stale decoder result must fail closed"
        );
        return;
    }
    if late_fragment {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "a late fragment must fail the run"
        );
        assert!(
            stderr.contains("booking A paid window elapsed during fragment handoff"),
            "static reason missing: {stderr}"
        );
        // The redacted status sequence is printed on failure: no ids or tokens.
        assert!(
            stderr.contains("http +"),
            "status sequence missing: {stderr}"
        );
        assert!(stderr.contains("/bookings/{booking}/grants"));
        assert!(!stderr.contains("bk:") && !stderr.contains("synthetic-"));
        return;
    }
    assert!(
        output.status.success(),
        "native command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("two takers"));
    if variant == "late-b-grants" {
        // The parked path must actually have run: objects parked and later opened.
        let out = String::from_utf8_lossy(&output.stdout);
        let report = out
            .lines()
            .find(|l| l.starts_with("credit-cutoff B late-grant report:"))
            .expect("late-grant report");
        let field = |name: &str| -> u64 {
            report
                .split_whitespace()
                .find_map(|w| w.strip_prefix(name))
                .and_then(|v| v.parse().ok())
                .unwrap()
        };
        assert!(field("parked=") > 0, "no B object was parked: {report}");
        assert!(field("drained=") > 0, "no parked B object opened: {report}");
    }
    if variant == "late-b-epoch" {
        // The unreachable pre-expiry epoch must have aged B's parked objects out
        // of the replay window (the empty-decode precondition), and B must have
        // decoded again afterwards: the run continued instead of aborting.
        let out = String::from_utf8_lossy(&output.stdout);
        let report = out
            .lines()
            .find(|l| l.starts_with("credit-cutoff B late-grant report:"))
            .expect("late-grant report");
        let field = |name: &str| -> u64 {
            report
                .split_whitespace()
                .find_map(|w| w.strip_prefix(name))
                .and_then(|v| v.parse().ok())
                .unwrap()
        };
        assert!(
            field("stale=") > 0,
            "no parked B object went stale: {report}"
        );
        assert!(
            field("drained=") > 0,
            "no parked B object opened after the stale epoch: {report}"
        );
        // The stale epoch must actually have emptied a track: this is the state
        // that used to be handed to the decoder and aborted the run.
        assert!(
            field("empty_decode_skips=") > 0,
            "the stale epoch never emptied a track: {report}"
        );
    }
    if scenario == "withdraw" {
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("withdraw report: publisher lease revoked (renew 404)"),
            "withdraw must prove the terminal lease cutoff"
        );
    }
}
