//! Focus-lease lifecycle: a slot is freed when its session dies without a client
//! `DELETE` (injected liveness, no wall-clock sleeps), caps come from config and
//! are reported by the capabilities document, and a same-owner re-offer replaces
//! its own stale session instead of locking the user out.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, FocusCaps, GatedWhep, InMemoryRepository, Principal, Role,
    WhepAnswer, WhepProvider, WhepReject, WhepScope,
};
use multiview_engine::EnginePublisher;

mod support;
use support::{body_json, get, send, OPERATOR_TOKEN, PEPPER};

const OFFER: &str = "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 H264/90000\r\n";

/// A fake transport whose liveness is injected: `kill(id)` simulates the viewer
/// vanishing (ICE failed / silent) and the next `reap_dead` reports it.
#[derive(Default)]
struct SimWhep {
    live: Mutex<Vec<String>>,
    dead: Mutex<HashSet<String>>,
    next: AtomicUsize,
}

impl SimWhep {
    fn kill(&self, id: &str) {
        self.dead.lock().unwrap().insert(id.to_owned());
    }
    fn live(&self) -> usize {
        self.live.lock().unwrap().len()
    }
}

impl WhepProvider for SimWhep {
    fn negotiate(&self, _scope: &WhepScope, _offer: &str) -> Result<WhepAnswer, WhepReject> {
        let id = format!("sess-{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.live.lock().unwrap().push(id.clone());
        Ok(WhepAnswer {
            session_id: id,
            sdp: "v=0\r\n".to_owned(),
        })
    }
    fn release(&self, _scope: &WhepScope, id: &str) -> bool {
        let mut l = self.live.lock().unwrap();
        let n = l.len();
        l.retain(|s| s != id);
        l.len() != n
    }
    fn active_sessions(&self) -> usize {
        self.live()
    }
    fn webrtc_available(&self) -> bool {
        true
    }
    fn reap_dead(&self, _now: Instant) -> Vec<String> {
        let dead: Vec<String> = self.dead.lock().unwrap().drain().collect();
        self.live.lock().unwrap().retain(|s| !dead.contains(s));
        dead
    }
}

fn router_for(gated: Arc<GatedWhep>) -> axum::Router {
    let engine = Arc::new(EnginePublisher::new(64));
    let (tx, _rx) = command_bus(4);
    let mut keys = ApiKeyStore::new(PEPPER.to_vec());
    for (id, secret, role) in [
        ("operator-key", "operator-secret-def", Role::Operator),
        ("admin-key", "admin-secret-abc", Role::Admin),
        ("viewer-key", "viewer-secret-ghi", Role::Viewer),
    ] {
        keys.register(
            id,
            secret,
            Principal {
                key_id: id.to_owned(),
                role,
                scoped_object_ids: None,
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
        );
    }
    let state = AppState::new(
        engine,
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    )
    .with_whep(gated);
    multiview_control::router(state)
}

fn post(path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/sdp")
        .body(Body::from(OFFER))
        .unwrap()
}

const PROGRAM: &str = "/api/v1/preview/program/whep";

#[tokio::test]
async fn a_session_that_dies_without_delete_frees_its_slot() {
    let sim = Arc::new(SimWhep::default());
    let gated = Arc::new(GatedWhep::new(sim.clone(), FocusCaps::new(1, 1), "jpeg"));
    let router = router_for(gated.clone());
    let first = send(&router, post(PROGRAM, "admin-key.admin-secret-abc")).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    // A different owner is shed: the single slot is held.
    let shed = send(&router, post(PROGRAM, OPERATOR_TOKEN)).await;
    assert_eq!(shed.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(shed).await["fallback"], "jpeg");
    // The viewer vanishes: no DELETE, the transport just reports it dead.
    sim.kill("sess-0");
    assert_eq!(gated.reap_dead(Instant::now()), vec!["sess-0".to_owned()]);
    assert_eq!(gated.active_sessions(), 0);
    let again = send(&router, post(PROGRAM, OPERATOR_TOKEN)).await;
    assert_eq!(
        again.status(),
        StatusCode::CREATED,
        "slot reusable after death"
    );
}

#[tokio::test]
async fn no_slot_leak_after_many_create_abandon_cycles() {
    let sim = Arc::new(SimWhep::default());
    let gated = Arc::new(GatedWhep::new(sim.clone(), FocusCaps::new(2, 2), "jpeg"));
    let router = router_for(gated.clone());
    for i in 0..50 {
        let r = send(&router, post(PROGRAM, "admin-key.admin-secret-abc")).await;
        assert_eq!(r.status(), StatusCode::CREATED, "cycle {i}");
        sim.kill(&format!("sess-{i}"));
        let _ = gated.reap_dead(Instant::now());
        assert_eq!(gated.active_sessions(), 0, "cycle {i} leaked a slot");
    }
    assert_eq!(sim.live(), 0);
}

#[tokio::test]
async fn same_owner_reoffer_replaces_its_own_stale_session() {
    let sim = Arc::new(SimWhep::default());
    let gated = Arc::new(GatedWhep::new(sim.clone(), FocusCaps::new(1, 1), "jpeg"));
    let router = router_for(gated.clone());
    let a = send(&router, post(PROGRAM, OPERATOR_TOKEN)).await;
    assert_eq!(a.status(), StatusCode::CREATED);
    // Reload: the same key offers again before deleting the old session.
    let b = send(&router, post(PROGRAM, OPERATOR_TOKEN)).await;
    assert_eq!(
        b.status(),
        StatusCode::CREATED,
        "own stale session is replaced"
    );
    assert_eq!(gated.active_sessions(), 1);
    assert_eq!(sim.live(), 1, "the old transport session was torn down");
    // Another owner can NOT evict the operator's session.
    let other = send(&router, post(PROGRAM, "admin-key.admin-secret-abc")).await;
    assert_eq!(other.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(gated.active_sessions(), 1);
}

#[tokio::test]
async fn reoffer_never_evicts_another_scope() {
    let sim = Arc::new(SimWhep::default());
    let gated = Arc::new(GatedWhep::new(sim, FocusCaps::new(1, 1), "jpeg"));
    let router = router_for(gated);
    let a = send(
        &router,
        post("/api/v1/preview/inputs/cam-1/whep", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(a.status(), StatusCode::CREATED);
    let b = send(&router, post(PROGRAM, OPERATOR_TOKEN)).await;
    assert_eq!(b.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn capabilities_report_configured_caps_and_occupancy() {
    let sim = Arc::new(SimWhep::default());
    let gated = Arc::new(GatedWhep::new(sim, FocusCaps::new(5, 2), "jpeg"));
    let router = router_for(gated);
    let _ = send(&router, post(PROGRAM, OPERATOR_TOKEN)).await;
    let r = send(&router, get("/api/v1/preview/capabilities", OPERATOR_TOKEN)).await;
    let caps = body_json(r).await;
    assert_eq!(caps["focus"]["max"], 5);
    assert_eq!(caps["focus"]["max_per_scope"], 2);
    assert_eq!(caps["focus"]["active"], 1);
}

#[tokio::test]
async fn cap_plus_one_gets_the_503_problem_shape() {
    let sim = Arc::new(SimWhep::default());
    let gated = Arc::new(GatedWhep::new(sim, FocusCaps::new(3, 3), "jpeg"));
    let router = router_for(gated);
    // Distinct owners so re-offer replacement cannot apply.
    for tok in [OPERATOR_TOKEN, "admin-key.admin-secret-abc"] {
        assert_eq!(
            send(&router, post(PROGRAM, tok)).await.status(),
            StatusCode::CREATED
        );
    }
    let third = send(
        &router,
        post("/api/v1/preview/inputs/a/whep", OPERATOR_TOKEN),
    )
    .await;
    assert_eq!(third.status(), StatusCode::CREATED);
    let over = send(
        &router,
        post(
            "/api/v1/preview/inputs/b/whep",
            "admin-key.admin-secret-abc",
        ),
    )
    .await;
    assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE);
    let p = body_json(over).await;
    assert_eq!(p["type"], "/problems/whep-capacity");
    assert_eq!(p["fallback"], "jpeg");
}
