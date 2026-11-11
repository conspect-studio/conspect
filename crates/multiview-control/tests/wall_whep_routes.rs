//! Whole-system wall WHEP authorization, typed refusal and release.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, InMemoryRepository, PreviewProvider, Principal, Role,
    WhepAnswer, WhepProvider, WhepReject, WhepScope,
};
use multiview_engine::EnginePublisher;
use std::sync::{Arc, Mutex};
mod support;
use support::{body_json, send, ADMIN_TOKEN, PEPPER, SCOPED_TOKEN, VIEWER_TOKEN};

struct Wall;
impl PreviewProvider for Wall {
    fn program_jpeg(&self, _: u8) -> Option<Vec<u8>> {
        None
    }
    fn input_jpeg(&self, _: &str, _: u8) -> Option<Vec<u8>> {
        None
    }
    fn input_ids(&self) -> Vec<String> {
        Vec::new()
    }
    fn wall(&self) -> Option<multiview_control::preview::WallGeometry> {
        Some(multiview_control::preview::WallGeometry {
            generation: 1,
            width: 64,
            height: 64,
            cadence: multiview_control::preview::WallCadence { num: 15, den: 1 },
            cells: Vec::new(),
            preview_source: None,
        })
    }
}
#[derive(Default)]
struct Focus(Mutex<bool>);
impl WhepProvider for Focus {
    fn negotiate(&self, scope: &WhepScope, _: &str) -> Result<WhepAnswer, WhepReject> {
        assert_eq!(*scope, WhepScope::Wall);
        *self.0.lock().unwrap() = true;
        Ok(WhepAnswer {
            session_id: "wall-test".into(),
            sdp: "v=0\r\n".into(),
        })
    }
    fn release(&self, scope: &WhepScope, id: &str) -> bool {
        if *scope != WhepScope::Wall || id != "wall-test" {
            return false;
        }
        std::mem::replace(&mut *self.0.lock().unwrap(), false)
    }
    fn active_sessions(&self) -> usize {
        usize::from(*self.0.lock().unwrap())
    }
    fn wall_available(&self) -> bool {
        true
    }
    fn webrtc_available(&self) -> bool {
        true
    }
}
fn router(wall: bool, focus: Arc<Focus>) -> axum::Router {
    let (tx, _rx) = command_bus(4);
    let mut keys = ApiKeyStore::new(PEPPER.to_vec());
    for (id, secret, role, scoped) in [
        ("admin-key", "admin-secret-abc", Role::Admin, false),
        ("viewer-key", "viewer-secret-ghi", Role::Viewer, false),
        ("scoped-key", "scoped-secret-jkl", Role::Operator, true),
    ] {
        keys.register(
            id,
            secret,
            Principal {
                key_id: id.into(),
                role,
                scoped_object_ids: scoped.then(|| vec!["one".into()]),
                scoped_output_ids: None,
                scoped_discovery_domains: None,
            },
        );
    }
    let state = AppState::new(
        Arc::new(EnginePublisher::new(64)),
        tx,
        Arc::new(InMemoryRepository::new()),
        Arc::new(keys),
    )
    .with_whep(focus);
    multiview_control::router(if wall {
        state.with_preview(Arc::new(Wall))
    } else {
        state
    })
}
fn request(method: &str, path: &str, token: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header("Authorization", format!("Bearer {token}"))
        .body(Body::from(if method == "POST" { "v=0\r\n" } else { "" }))
        .unwrap()
}
#[tokio::test]
async fn wall_whep_requires_whole_system_write_and_a_ready_wall() {
    let focus = Arc::new(Focus::default());
    let absent = router(false, focus.clone());
    let path = "/api/v1/preview/wall/whep";
    let denied = send(&absent, request("POST", path, SCOPED_TOKEN)).await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let denied = send(&absent, request("POST", path, VIEWER_TOKEN)).await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let unavailable = send(&absent, request("POST", path, ADMIN_TOKEN)).await;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body_json(unavailable).await["type"],
        "/problems/wall-unavailable"
    );
    assert_eq!(focus.active_sessions(), 0);
    let ready = router(true, focus.clone());
    let opened = send(&ready, request("POST", path, ADMIN_TOKEN)).await;
    assert_eq!(opened.status(), StatusCode::CREATED);
    let location = opened
        .headers()
        .get("Location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(location, "/api/v1/preview/wall/whep/wall-test");
    assert_eq!(focus.active_sessions(), 1);
    let denied = send(&ready, request("DELETE", &location, SCOPED_TOKEN)).await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(focus.active_sessions(), 1);
    let closed = send(&ready, request("DELETE", &location, ADMIN_TOKEN)).await;
    assert_eq!(closed.status(), StatusCode::NO_CONTENT);
    assert_eq!(focus.active_sessions(), 0);
}
