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
    scenario("release", false, false);
}
#[test]
fn withdraw_command_cuts_off_both_takers() {
    scenario("withdraw", false, false);
}
#[test]
fn exhaustion_command_uses_decoded_receipts() {
    scenario("exhaustion", false, false);
}
#[test]
fn slow_native_decoder_keeps_tls_lease_and_bookings_alive() {
    scenario("release", true, false);
}
#[test]
fn lease_denial_flushes_while_native_decoder_stalls() {
    scenario("release", true, true);
}
#[test]
fn raw_root_identity_mismatch_fails_closed() {
    scenario("raw-mismatch", false, false);
}
#[test]
fn missing_authority_window_fails_closed() {
    scenario("missing-window", false, false);
}
#[test]
fn late_authority_headers_fail_closed() {
    scenario("late-root", false, false);
}
#[test]
fn credit_cutoff_uses_early_credit_refusal_and_paid_boundary() {
    scenario("credit-cutoff", false, false);
}
fn scenario(scenario: &'static str, slow: bool, deny_renew: bool) {
    // Admit one six-stream FFmpeg/TLS fixture at a time. These wall-clock
    // integration scenarios have real five-second grants; eight concurrent
    // encoders/decoders can miss their epoch under host contention. This bounds
    // test resource demand without changing any authority deadline or assertion.
    static ADMISSION: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _admission = ADMISSION.lock().unwrap_or_else(|p| p.into_inner());
    let negative = matches!(scenario, "raw-mismatch" | "missing-window" | "late-root");
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
                // The token is minted once, on acquire, and never appears on a
                // renewal answer.
                json!({"generation":793,"leaseToken":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","leaseExpiresAtMs":now()+10000,"notBeforeMs":1})
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
                json!({"generation":793,"leaseExpiresAtMs":now()+10000,"notBeforeMs":1})
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
                        10000
                    } else {
                        60000
                    }
                );
                let created = now();
                expiry[i] = created + body["durationMs"].as_u64().unwrap();
                books.push(id.clone());
                if books.len() == 2 {
                    let v = json!({"kind":"exchange-connected-proof-expected-fresh","shared":{"contributorOrg":URL_SAFE_NO_PAD.encode([10;16]),"publisherInstance":URL_SAFE_NO_PAD.encode([5;16]),"feed":URL_SAFE_NO_PAD.encode([9;16]),"version":URL_SAFE_NO_PAD.encode([3;16]),"publisherMachine":URL_SAFE_NO_PAD.encode([5;16])},"bookingA":{"id":books[0],"booking":URL_SAFE_NO_PAD.encode([1;16]),"takerOrg":URL_SAFE_NO_PAD.encode([2;16]),"receiverMachine":URL_SAFE_NO_PAD.encode([4;16])},"bookingB":{"id":books[1],"booking":URL_SAFE_NO_PAD.encode([6;16]),"takerOrg":URL_SAFE_NO_PAD.encode([7;16]),"receiverMachine":URL_SAFE_NO_PAD.encode([8;16])}});
                    std::fs::write(&expected_server, serde_json::to_vec(&v).unwrap()).unwrap();
                    std::fs::set_permissions(
                        &expected_server,
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                }
                json!({"id":id,"created_at":created,"expires_at":expiry[i]})
            } else if path.contains("/current-epoch?") {
                // No seal may follow a withdrawal: the engine must stop at once.
                assert!(
                    !(scenario == "withdraw" && denied),
                    "root fetched after the withdrawal (sealing continued)"
                );
                roots += 1;
                let mut dto = json!({"versionId":"ver_test","publisherMachineId":"mac_pub","generation":793,"epoch":now()/5000,"epochStartMs":now()/5000*5000,"epochEndMs":(now()/5000+1)*5000,"feedCryptoId":URL_SAFE_NO_PAD.encode([9;16]),"versionCryptoId":URL_SAFE_NO_PAD.encode([3;16]),"publisherMachineCryptoId":URL_SAFE_NO_PAD.encode([5;16]),"contributorOrgCryptoId":URL_SAFE_NO_PAD.encode([10;16]),"rootKeyBase64":STANDARD.encode([11;32])});
                if scenario == "raw-mismatch" {
                    dto["feedCryptoId"] = json!(URL_SAFE_NO_PAD.encode([99; 16]));
                }
                if scenario == "missing-window" {
                    dto.as_object_mut().unwrap().remove("epochEndMs");
                }
                if scenario == "late-root" {
                    std::thread::sleep(std::time::Duration::from_millis(5000 - now() % 5000 + 20));
                }
                dto
            } else if path.ends_with("/grants") {
                if scenario == "credit-cutoff" && path.contains("org_a/") && now() >= expiry[0] {
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
            let sent = (|| -> std::io::Result<()> {
                write!(tls,"HTTP/1.1 {status} OK\r\ncontent-type: {typ}\r\ncontent-length: {}\r\ncache-control: no-store, private\r\nconnection: close\r\n\r\n",bytes.len())?;
                tls.write_all(&bytes)?;
                tls.flush()
            })();
            if let Err(error) = sent {
                // A deliberately late response can outlive the client's deadline.
                // All negative-path receipt and command assertions still apply.
                assert!(
                    scenario == "late-root"
                        && matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        )
                );
            }
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
        manifest["scenario"]["fixtureProfile"] = json!({"name":"funded-expiry"});
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
    assert!(
        output.status.success(),
        "native command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("two takers"));
    if scenario == "withdraw" {
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("withdraw report: publisher lease revoked (renew 404)"),
            "withdraw must prove the terminal lease cutoff"
        );
    }
}
