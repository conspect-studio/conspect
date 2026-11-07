//! Review R2-01 / R2-02 (PR #190): the store-served families (overlays, probes,
//! devices, sync groups, salvos, tally profiles) answer reads and preconditions
//! from the accepted envelope, and their stores carry the envelope's exact
//! versions.
//!
//! * R2-01: a store write that fails after the durable commit must not leave a
//!   pre-commit `ETag` able to overwrite the committed body.
//! * R2-02: deleting and recreating an identity continues its durable counter,
//!   so an old incarnation's tag is refused before and after a restart, and a
//!   restart serves the same tag.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod support;

use axum::http::StatusCode;
use axum::Router;
use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::{
    boot_model::{load_desired, BootModel},
    command_bus,
    salvo_store::{InMemorySalvoStore, SalvoRepository, VersionedSalvo},
    tally_state::{InMemoryProfileStore, TallyProfileRepository, VersionedProfile},
    AppState, ControlError, ControlResult, EngineStateSnapshot, InMemoryRepository, ResourceInput,
    ResourceRepository, VersionedResource,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use support::{body_json, delete_if_match, etag, get, post_json, put_json, send, ADMIN_TOKEN};

const BOOT: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/desired-store-served-authority.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;

fn fault() -> ControlError {
    ControlError::Repository("injected store write fault".into())
}

/// A store that reads normally but fails every write while `fail` is set.
struct WriteFault<S> {
    inner: S,
    fail: Arc<AtomicBool>,
}

impl<S> WriteFault<S> {
    fn check(&self) -> ControlResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            Err(fault())
        } else {
            Ok(())
        }
    }
}

impl<S: ResourceRepository> ResourceRepository for WriteFault<S> {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn list(&self) -> ControlResult<Vec<VersionedResource>> {
        self.inner.list()
    }
    fn get(&self, id: &str) -> ControlResult<VersionedResource> {
        self.inner.get(id)
    }
    fn create(&self, id: &str, input: ResourceInput) -> ControlResult<VersionedResource> {
        self.check()?;
        self.inner.create(id, input)
    }
    fn update(&self, id: &str, input: ResourceInput) -> ControlResult<VersionedResource> {
        self.check()?;
        self.inner.update(id, input)
    }
    fn delete(&self, id: &str) -> ControlResult<()> {
        self.check()?;
        self.inner.delete(id)
    }
    fn put_versioned(&self, entry: VersionedResource) -> ControlResult<VersionedResource> {
        self.check()?;
        self.inner.put_versioned(entry)
    }
}

impl SalvoRepository for WriteFault<InMemorySalvoStore> {
    fn list(&self) -> ControlResult<Vec<VersionedSalvo>> {
        self.inner.list()
    }
    fn get(&self, id: &str) -> ControlResult<VersionedSalvo> {
        self.inner.get(id)
    }
    fn create(&self, salvo: multiview_config::Salvo) -> ControlResult<VersionedSalvo> {
        self.check()?;
        self.inner.create(salvo)
    }
    fn update(&self, id: &str, salvo: multiview_config::Salvo) -> ControlResult<VersionedSalvo> {
        self.check()?;
        self.inner.update(id, salvo)
    }
    fn delete(&self, id: &str) -> ControlResult<()> {
        self.check()?;
        self.inner.delete(id)
    }
    fn put_versioned(&self, entry: VersionedSalvo) -> ControlResult<VersionedSalvo> {
        self.check()?;
        self.inner.put_versioned(entry)
    }
}

impl TallyProfileRepository for WriteFault<InMemoryProfileStore> {
    fn list(&self) -> ControlResult<Vec<VersionedProfile>> {
        self.inner.list()
    }
    fn get(&self, id: &str) -> ControlResult<VersionedProfile> {
        self.inner.get(id)
    }
    fn put(&self, profile: multiview_config::TallyProfile) -> ControlResult<VersionedProfile> {
        self.check()?;
        self.inner.put(profile)
    }
    fn delete(&self, id: &str) -> ControlResult<()> {
        self.check()?;
        self.inner.delete(id)
    }
    fn put_versioned(&self, entry: VersionedProfile) -> ControlResult<VersionedProfile> {
        self.check()?;
        self.inner.put_versioned(entry)
    }
}

fn state_for(path: &Path, config: &MultiviewConfig) -> AppState {
    let seeded = multiview_control::seed_resources(config).unwrap();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (sender, _receiver) = command_bus(64);
    AppState::new(
        publisher,
        sender,
        Arc::new(InMemoryRepository::new()),
        Arc::new(support::seeded_keys()),
    )
    .with_seeded_resources(seeded)
    .with_base_document(serde_json::to_value(config).unwrap())
    .with_boot_model(Arc::new(BootModel::new(
        path.to_path_buf(),
        config.clone(),
        config.clone(),
        StartMode::Boot,
        false,
        None,
    )))
}

/// A run with an accepted envelope whose store-served stores can be made to
/// fail every write.
async fn faultable() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    Router,
    Arc<AtomicBool>,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("boot.toml");
    std::fs::write(&path, BOOT).unwrap();
    let config = MultiviewConfig::load_from_toml(BOOT).unwrap();
    let mut state = state_for(&path, &config);
    let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
    let initial: MultiviewConfig = serde_json::from_value(
        multiview_control::boot_model::current_desired_config(&state).unwrap(),
    )
    .unwrap();
    multiview_control::boot_model::commit_desired(&state, initial, layouts)
        .await
        .unwrap();
    let fail = Arc::new(AtomicBool::new(false));
    let wrap = |store: &Arc<dyn ResourceRepository>| -> Arc<dyn ResourceRepository> {
        // The seeded stores are empty for these families in BOOT.
        assert_eq!(store.list().unwrap().len(), 0);
        match store.kind() {
            "overlay" => Arc::new(WriteFault {
                inner: multiview_control::resource_store::InMemoryOverlayStore::new(),
                fail: Arc::clone(&fail),
            }),
            "probe" => Arc::new(WriteFault {
                inner: multiview_control::resource_store::InMemoryProbeStore::new(),
                fail: Arc::clone(&fail),
            }),
            "device" => Arc::new(WriteFault {
                inner: multiview_control::resource_store::InMemoryDeviceStore::new(),
                fail: Arc::clone(&fail),
            }),
            _ => Arc::new(WriteFault {
                inner: multiview_control::resource_store::InMemorySyncGroupStore::new(),
                fail: Arc::clone(&fail),
            }),
        }
    };
    state.overlays = wrap(&state.overlays);
    state.probes = wrap(&state.probes);
    state.devices = wrap(&state.devices);
    state.sync_groups = wrap(&state.sync_groups);
    state.salvos = Arc::new(WriteFault {
        inner: InMemorySalvoStore::new(),
        fail: Arc::clone(&fail),
    });
    state.tally_profiles = Arc::new(WriteFault {
        inner: InMemoryProfileStore::new(),
        fail: Arc::clone(&fail),
    });
    (dir, path, multiview_control::router(state), fail)
}

fn restart(path: &Path) -> Router {
    let desired = load_desired(path).unwrap().expect("accepted envelope");
    let state = multiview_control::boot_model::restore_accepted_stores(
        state_for(path, &desired.config),
        &desired,
    )
    .unwrap();
    multiview_control::router(state)
}

/// The tag a write returned, after asserting its status.
fn ok_tag(
    response: &axum::http::Response<axum::body::Body>,
    want: StatusCode,
    what: &str,
) -> String {
    assert_eq!(response.status(), want, "{what}");
    etag(response).unwrap_or_else(|| panic!("{what}: no ETag"))
}

/// Body of a GET, unwrapping the resource envelope for resource families.
async fn read(router: &Router, uri: &str) -> (StatusCode, Option<String>, Value) {
    let response = send(router, get(uri, ADMIN_TOKEN)).await;
    let status = response.status();
    let tag = etag(&response);
    let value = body_json(response).await;
    (status, tag, value.get("body").cloned().unwrap_or(value))
}

/// One resource-family case: `create`/`edit` are request bodies, `field` and
/// `edited` identify the committed edit in the GET body.
struct Family {
    uri: &'static str,
    create: Value,
    edit: Value,
    field: &'static str,
    edited: Value,
    /// Salvos and tally profiles create with PUT (no If-Match), the rest POST.
    put_create: bool,
}

fn families() -> Vec<Family> {
    let device = |octet: &str| {
        json!({"name": "dev-a", "body": {"id": "dev-a", "driver": "zowietek",
            "address": format!("http://[fd00:db8::{octet}]"), "desired_mode": "decoder"}})
    };
    let probe = |threshold: u32| {
        json!({"name": "p", "body": {
            "id": "probe-1", "cell": "cell_a", "kind": "black", "luma_threshold": threshold,
            "dwell": {"up_ms": 2000, "down_ms": 1000}, "severity": "Major", "latched": false
        }})
    };
    let group = |skew: u32| {
        json!({"name": "g", "body": {"id": "wall", "mode": "auto", "target_skew_ms": skew,
            "members": [{"device": "dev-member", "offset_ms": 0}]}})
    };
    vec![
        Family {
            uri: "/api/v1/overlays/ov_clock",
            create: json!({"name": "c", "body": {"kind": "clock", "target": "canvas"}}),
            edit: json!({"name": "c", "body": {"kind": "clock", "target": "canvas", "format": "%H:%M"}}),
            field: "format",
            edited: json!("%H:%M"),
            put_create: false,
        },
        Family {
            uri: "/api/v1/probes/probe-1",
            create: probe(16),
            edit: probe(20),
            field: "luma_threshold",
            edited: json!(20),
            put_create: false,
        },
        Family {
            uri: "/api/v1/devices/dev-a",
            create: device("41"),
            edit: device("99"),
            field: "address",
            edited: json!("http://[fd00:db8::99]"),
            put_create: false,
        },
        Family {
            uri: "/api/v1/sync-groups/wall",
            create: group(50),
            edit: group(70),
            field: "target_skew_ms",
            edited: json!(70),
            put_create: false,
        },
        Family {
            uri: "/api/v1/salvos/wide",
            create: json!({"id": "wide", "display_name": "Wide", "tally": [{"cell": "cell_a", "color": "Red"}]}),
            edit: json!({"id": "wide", "display_name": "Wide v2", "tally": [{"cell": "cell_a", "color": "Red"}]}),
            field: "display_name",
            edited: json!("Wide v2"),
            put_create: true,
        },
        Family {
            uri: "/api/v1/tally/profiles/main",
            create: json!({"id": "main", "bit_colors": [{"bit": 0, "color": "Red"}], "index_cells": [{"index": 0, "cell": "cell_a"}]}),
            edit: json!({"id": "main", "bit_colors": [{"bit": 0, "color": "Green"}], "index_cells": [{"index": 0, "cell": "cell_a"}]}),
            field: "bit_colors",
            edited: json!([{"bit": 0, "color": "Green"}]),
            put_create: true,
        },
    ]
}

/// The device every sync-group case references.
async fn member_device(router: &Router) {
    let response = send(
        router,
        post_json(
            "/api/v1/devices/dev-member",
            ADMIN_TOKEN,
            &json!({"name": "m", "body": {"id": "dev-member", "driver": "zowietek",
                "address": "http://[fd00:db8::50]", "desired_mode": "decoder"}}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

async fn create(router: &Router, family: &Family, body: &Value) -> String {
    let response = if family.put_create {
        send(router, put_json(family.uri, ADMIN_TOKEN, None, body)).await
    } else {
        send(router, post_json(family.uri, ADMIN_TOKEN, body)).await
    };
    ok_tag(&response, StatusCode::CREATED, family.uri)
}

/// R2-01: after a commit whose store write fails, the old tag is refused with
/// 412 and GET returns the committed body and tag, for every store-served family.
#[tokio::test]
async fn failed_store_write_after_commit_never_authorizes_a_stale_tag() {
    let (_dir, _path, router, fail) = faultable().await;
    member_device(&router).await;
    for family in families() {
        let old = create(&router, &family, &family.create).await;
        fail.store(true, Ordering::SeqCst);
        let committed = send(
            &router,
            put_json(family.uri, ADMIN_TOKEN, Some(&old), &family.edit),
        )
        .await;
        fail.store(false, Ordering::SeqCst);
        let fresh = ok_tag(&committed, StatusCode::OK, family.uri);
        assert_ne!(fresh, old, "{}", family.uri);
        let (status, tag, body) = read(&router, family.uri).await;
        assert_eq!(status, StatusCode::OK, "{}", family.uri);
        assert_eq!(
            tag.as_deref(),
            Some(fresh.as_str()),
            "{}: GET tag",
            family.uri
        );
        assert_eq!(
            body[family.field], family.edited,
            "{}: GET body",
            family.uri
        );
        let stale = send(
            &router,
            put_json(family.uri, ADMIN_TOKEN, Some(&old), &family.create),
        )
        .await;
        assert_eq!(
            stale.status(),
            StatusCode::PRECONDITION_FAILED,
            "{}: a pre-commit tag must not overwrite committed intent",
            family.uri
        );
        // A failed delete projection: GET reports the accepted absence and the
        // recreate is a create, not a replace of a ghost.
        fail.store(true, Ordering::SeqCst);
        let deleted = send(
            &router,
            delete_if_match(family.uri, ADMIN_TOKEN, Some(&fresh)),
        )
        .await;
        fail.store(false, Ordering::SeqCst);
        assert!(
            deleted.status().is_success(),
            "{}: {}",
            family.uri,
            deleted.status()
        );
        let (status, _, _) = read(&router, family.uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{}: deleted", family.uri);
    }
    // A failed create projection: the accepted object exists and a second
    // create of the same id is refused instead of replacing it.
    let family = &families()[0];
    fail.store(true, Ordering::SeqCst);
    let created = send(&router, post_json(family.uri, ADMIN_TOKEN, &family.edit)).await;
    fail.store(false, Ordering::SeqCst);
    let tag = ok_tag(&created, StatusCode::CREATED, family.uri);
    let (status, served, body) = read(&router, family.uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(served, Some(tag));
    assert_eq!(body[family.field], family.edited);
    let again = send(&router, post_json(family.uri, ADMIN_TOKEN, &family.create)).await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// R2-02: delete then recreate continues the durable counter; the previous
/// incarnation's tag is refused before and after restart, and a restart
/// serves the identical tag.
#[tokio::test]
async fn recreated_identity_never_reuses_a_tag_and_keeps_it_across_restart() {
    let (_dir, path, router, _fail) = faultable().await;
    member_device(&router).await;
    for family in families() {
        let first = create(&router, &family, &family.create).await;
        let deleted = send(
            &router,
            delete_if_match(family.uri, ADMIN_TOKEN, Some(&first)),
        )
        .await;
        assert!(deleted.status().is_success(), "{}", family.uri);
        let second = create(&router, &family, &family.edit).await;
        assert_ne!(
            second, first,
            "{}: a recreate reused the old tag",
            family.uri
        );
        let (_, served, _) = read(&router, family.uri).await;
        assert_eq!(served.as_deref(), Some(second.as_str()), "{}", family.uri);
        let stale = send(
            &router,
            put_json(family.uri, ADMIN_TOKEN, Some(&first), &family.create),
        )
        .await;
        assert_eq!(
            stale.status(),
            StatusCode::PRECONDITION_FAILED,
            "{}: the old incarnation's tag must be refused",
            family.uri
        );
        let restarted = restart(&path);
        let (status, after, body) = read(&restarted, family.uri).await;
        assert_eq!(status, StatusCode::OK, "{}", family.uri);
        assert_eq!(
            after,
            Some(second.clone()),
            "{}: restart moved the tag",
            family.uri
        );
        assert_eq!(body[family.field], family.edited, "{}", family.uri);
        let stale = send(
            &restarted,
            put_json(family.uri, ADMIN_TOKEN, Some(&first), &family.create),
        )
        .await;
        assert_eq!(
            stale.status(),
            StatusCode::PRECONDITION_FAILED,
            "{}",
            family.uri
        );
    }
}
