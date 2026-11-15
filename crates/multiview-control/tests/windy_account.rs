#![cfg(unix)]
//! Windy Premium account (issue #470): admin-only per-object authz, write-only
//! secret (never in any response, audit entry, log line or plaintext file),
//! Premium gated on a passing verify of the current credential, fail-closed
//! capture, and the credential reaching the browser host over stdin only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use axum::http::StatusCode;
use multiview_control::web_capture::{
    windy_account::{Verifier, WindyAccount},
    Provider, WindyVerdict,
};
use multiview_control::web_capture_store::{DurableTerms, Ledger};
use multiview_webengine::{
    artifact::{ArtifactStore, Limits},
    capture::Tools,
    service::{Config, Service},
};
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use support::{
    body_bytes, get, harness_with, send, ADMIN_TOKEN, OPERATOR_TOKEN, TALLY_SCOPED_ADMIN_TOKEN,
    VIEWER_TOKEN,
};

const EMAIL: &str = "windy-ops-7f3a@example.com";
const PASSWORD: &str = "Pw-6c1e9b2d-NEVER-ECHO";

/// Every tracing line the process emits, for the leak scan.
fn logs() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(buffer.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    })
}
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Rig {
    root: tempfile::TempDir,
    provider: Provider,
    verdict: Arc<Mutex<WindyVerdict>>,
    calls: Arc<Mutex<Vec<String>>>,
}
fn private(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path.to_owned()
}
/// A real capture service whose fake host records argv, environment and stdin
/// separately, so the test can prove where the credential travelled.
fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let seen = private(&r.join("seen"));
    let node = r.join("node");
    std::fs::write(
        &node,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{s}/argv'\nenv > '{s}/env'\ncat > '{s}/stdin'\n\
             printf 'producer-bytes'\nprintf '%s\\n' '{{\"event\":\"capture_complete\",\"frames\":2}}' >&2\n",
            s = seen.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    let artifacts = private(&r.join("artifacts"));
    let store = ArtifactStore::open(
        &artifacts,
        Limits {
            bytes: 1024 * 1024 * 1024,
            count: 8,
        },
    )
    .unwrap();
    let service = Service::start_durable(
        Tools {
            node,
            browser: "/bin/sh".into(),
            ffmpeg: "/bin/sh".into(),
            gl: multiview_webengine::capture::BrowserGl::Default,
        },
        store,
        Config {
            queue_capacity: 2,
            history: 8,
            max_fills: 4,
        },
        &r.join("journal"),
    )
    .unwrap();
    let verdict = Arc::new(Mutex::new(WindyVerdict::Rejected));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (v, c) = (verdict.clone(), calls.clone());
    let verifier: Verifier = Arc::new(move |credentials| {
        c.lock().unwrap().push(credentials.password.clone());
        *v.lock().unwrap()
    });
    let account = WindyAccount::open(&private(&r.join("windy")), [5; 32], verifier).unwrap();
    let provider = Provider::configured(
        Arc::new(service),
        Arc::new(DurableTerms::open(&r.join("terms.log")).unwrap()),
        Arc::new(Ledger::durable(&r.join("idem.log")).unwrap()),
    )
    .with_windy_account(Arc::new(account));
    Rig {
        root,
        provider,
        verdict,
        calls,
    }
}

fn request(
    method: &str,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> axum::http::Request<axum::body::Body> {
    let builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("idempotency-key", format!("k-{}", uuid_like()));
    builder
        .body(body.map_or_else(axum::body::Body::empty, |b| {
            axum::body::Body::from(b.to_string())
        }))
        .unwrap()
}
fn uuid_like() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}
fn premium_capture() -> serde_json::Value {
    json!({"fill_id":"radar","view":{"lat":-33.9,"lon":150.4,"zoom":8,"overlay":"wind","width":1280,"height":720,"fps":2,"frames":2,"public":false,"capture":"realtime"},"terms":{"accepted":true,"version":"windy-2023-09-01"}})
}
const ACCOUNT: &str = "/api/v1/integrations/windy";
const VERIFY: &str = "/api/v1/integrations/windy/verify";

/// Collects every response body for the final secret scan.
struct Seen(Vec<u8>);
impl Seen {
    async fn send(
        &mut self,
        router: &axum::Router,
        req: axum::http::Request<axum::body::Body>,
    ) -> (StatusCode, serde_json::Value) {
        let response = send(router, req).await;
        let status = response.status();
        for (name, value) in response.headers() {
            self.0.extend_from_slice(name.as_str().as_bytes());
            self.0.extend_from_slice(value.as_bytes());
        }
        let bytes = body_bytes(response).await;
        self.0.extend_from_slice(&bytes);
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }
}
fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}
fn tree_contains(dir: &Path, needle: &str, skip: &Path) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.starts_with(skip) {
            continue;
        }
        if path.is_dir() {
            hits.extend(tree_contains(&path, needle, skip));
        } else if contains(&std::fs::read(&path).unwrap_or_default(), needle) {
            hits.push(path);
        }
    }
    hits
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // One account lifecycle, scanned once at the end.
async fn secret_is_write_only_and_premium_needs_a_passing_verify_of_the_current_credential() {
    let logs = logs().clone();
    let rig = rig();
    let h = harness_with(|s| s.with_web_capture(rig.provider.clone()));
    let mut seen = Seen(Vec::new());
    let caps = || get("/api/v1/media/captures/capabilities", VIEWER_TOKEN);

    // Unset: GET shows set:false; Premium capture fails closed with a clear 409.
    let (status, view) = seen
        .send(&h.router, request("GET", ACCOUNT, ADMIN_TOKEN, None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["set"], false);
    let (status, problem) = seen
        .send(
            &h.router,
            request(
                "POST",
                "/api/v1/media/captures",
                OPERATOR_TOKEN,
                Some(&premium_capture()),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{problem}");
    assert!(problem["detail"]
        .as_str()
        .unwrap()
        .contains("Windy Premium is not available"));

    // Store: the response is secret-free and Premium is still off.
    let body = json!({"email": EMAIL, "password": PASSWORD});
    let (status, view) = seen
        .send(&h.router, request("PUT", ACCOUNT, ADMIN_TOKEN, Some(&body)))
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(
        view,
        json!({"set":true,"last_verified_at":null,"result":null,"premium_available":false,"verifying":false})
    );
    let (_, c) = seen.send(&h.router, caps()).await;
    assert_eq!(c["premium_available"], false);

    // Wrong password: typed verdict, Premium stays off, capture still refused.
    let (status, result) = seen
        .send(&h.router, request("POST", VERIFY, ADMIN_TOKEN, None))
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["result"], "rejected");
    assert_eq!(result["account"]["result"], "rejected");
    assert!(result["account"]["last_verified_at"].is_string());
    assert_eq!(result["account"]["premium_available"], false);
    let (_, c) = seen.send(&h.router, caps()).await;
    assert_eq!(c["premium_available"], false);
    let (status, _) = seen
        .send(
            &h.router,
            request(
                "POST",
                "/api/v1/media/captures",
                OPERATOR_TOKEN,
                Some(&premium_capture()),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Passing verify of the stored credential enables Premium.
    *rig.verdict.lock().unwrap() = WindyVerdict::Premium;
    let (_, result) = seen
        .send(&h.router, request("POST", VERIFY, ADMIN_TOKEN, None))
        .await;
    assert_eq!(result["result"], "premium");
    assert_eq!(rig.calls.lock().unwrap().as_slice(), [PASSWORD, PASSWORD]);
    let (_, c) = seen.send(&h.router, caps()).await;
    assert_eq!(c["premium_available"], true);

    // A Premium capture runs; the credential reaches the host on stdin only.
    let (status, accepted) = seen
        .send(
            &h.router,
            request(
                "POST",
                "/api/v1/media/captures",
                OPERATOR_TOKEN,
                Some(&premium_capture()),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    let id = accepted["job_id"].as_u64().unwrap();
    let mut state = String::new();
    for _ in 0..400 {
        let (_, job) = seen
            .send(
                &h.router,
                get(&format!("/api/v1/media/captures/{id}"), OPERATOR_TOKEN),
            )
            .await;
        state = job["state"].as_str().unwrap_or_default().to_owned();
        if !matches!(state.as_str(), "queued" | "running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(state, "succeeded");
    let seen_dir = rig.root.path().join("seen");
    let stdin: serde_json::Value =
        serde_json::from_slice(&std::fs::read(seen_dir.join("stdin")).unwrap()).unwrap();
    let view = &stdin["source"]["view"];
    assert_eq!(view["require_premium"], true);
    assert_eq!(view["credentials"]["username"], EMAIL);
    assert_eq!(view["credentials"]["password"], PASSWORD);
    for channel in ["argv", "env"] {
        let bytes = std::fs::read(seen_dir.join(channel)).unwrap();
        assert!(
            !contains(&bytes, PASSWORD) && !contains(&bytes, EMAIL),
            "{channel} leaked"
        );
    }

    // Changing the credential resets Premium.
    let other = json!({"email": EMAIL, "password": "Another-Pw-123"});
    let (_, view) = seen
        .send(
            &h.router,
            request("PUT", ACCOUNT, ADMIN_TOKEN, Some(&other)),
        )
        .await;
    assert_eq!(view["premium_available"], false);
    assert_eq!(view["result"], serde_json::Value::Null);
    let (_, c) = seen.send(&h.router, caps()).await;
    assert_eq!(c["premium_available"], false);
    let (status, _) = seen
        .send(
            &h.router,
            request(
                "POST",
                "/api/v1/media/captures",
                OPERATOR_TOKEN,
                Some(&premium_capture()),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // The audit trail records the changes, never the secret.
    let (status, audit) = seen
        .send(&h.router, get("/api/v1/audit", ADMIN_TOKEN))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(audit.to_string().contains("windy"), "{audit}");
    let (status, view) = seen
        .send(&h.router, request("DELETE", ACCOUNT, ADMIN_TOKEN, None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["set"], false);

    // Scan: responses, audit, logs and every file except the host's own stdin record.
    for needle in [PASSWORD, EMAIL, "Another-Pw-123"] {
        assert!(!contains(&seen.0, needle), "API response leaked {needle}");
        assert!(
            !contains(&logs.lock().unwrap(), needle),
            "log leaked {needle}"
        );
        let hits = tree_contains(rig.root.path(), needle, &seen_dir);
        assert!(hits.is_empty(), "plaintext at rest: {hits:?}");
    }
}

#[tokio::test]
async fn only_an_unscoped_admin_may_touch_the_windy_account() {
    let rig = rig();
    let h = harness_with(|s| s.with_web_capture(rig.provider.clone()));
    let body = json!({"email": EMAIL, "password": PASSWORD});
    // Operators/viewers (role) and a scoped admin (foreign object scope) are denied.
    for token in [VIEWER_TOKEN, OPERATOR_TOKEN, TALLY_SCOPED_ADMIN_TOKEN] {
        for (method, path, b) in [
            ("GET", ACCOUNT, None),
            ("PUT", ACCOUNT, Some(&body)),
            ("DELETE", ACCOUNT, None),
            ("POST", VERIFY, None),
        ] {
            let r = send(&h.router, request(method, path, token, b)).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method} {path} {token}");
        }
    }
    let r = send(&h.router, request("GET", ACCOUNT, "nobody.nothing", None)).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // Denied writes changed nothing.
    let r = send(&h.router, request("GET", ACCOUNT, ADMIN_TOKEN, None)).await;
    let view: serde_json::Value = serde_json::from_slice(&body_bytes(r).await).unwrap();
    assert_eq!(view["set"], false);
    assert!(rig.calls.lock().unwrap().is_empty());
    // Verify with nothing stored is a clear conflict, not a run.
    let r = send(&h.router, request("POST", VERIFY, ADMIN_TOKEN, None)).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    // Malformed input is a typed validation error and never echoed.
    let bad = json!({"email": "no-at-sign", "password": PASSWORD});
    let r = send(&h.router, request("PUT", ACCOUNT, ADMIN_TOKEN, Some(&bad))).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!contains(&body_bytes(r).await, PASSWORD));
}

#[tokio::test]
async fn without_an_account_store_premium_is_unavailable_and_the_routes_say_so() {
    let h = harness_with(|s| s);
    let r = send(&h.router, request("GET", ACCOUNT, ADMIN_TOKEN, None)).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let r = send(
        &h.router,
        get("/api/v1/media/captures/capabilities", VIEWER_TOKEN),
    )
    .await;
    let caps: serde_json::Value = serde_json::from_slice(&body_bytes(r).await).unwrap();
    assert_eq!(caps["premium_available"], false);
}
