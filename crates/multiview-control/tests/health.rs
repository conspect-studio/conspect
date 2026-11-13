//! Health-warning REST surface + ingest tests (tower oneshot): `GET
//! /api/v1/health` lists active warnings, RBAC (a viewer reads), the
//! engine→control ingest wiring (a `health.warning.raised` event flows through
//! the ingest into the store the router reads), the **latched/idempotent**
//! property (emitting the same warning twice yields one active entry), clearing,
//! and the isolation property (a slow ingest lags rather than back-pressuring the
//! engine). SA-0 / ADR-0035.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use axum::http::StatusCode;
use multiview_control::{warning_ingest_step, WarningIngestStep};
use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};
use support::{body_json, get, harness, send, VIEWER_TOKEN};

fn warning(active: bool) -> HealthWarning {
    HealthWarning {
        code: WarningCode::GpuPresentNoVulkanAdapter,
        severity: WarningSeverity::Warning,
        subsystem: "compositor".to_owned(),
        message: "GPU NVIDIA GeForce RTX 4060 detected (NVML) but GPU compositing \
                  is UNAVAILABLE (no Vulkan adapter); compositing fell back to CPU."
            .to_owned(),
        remediation: "Set NVIDIA_DRIVER_CAPABILITIES to include `graphics` (or `all`) \
                      and install `libvulkan1` + the `nvidia_icd.json` ICD."
            .to_owned(),
        since: 1_700_000_000_000_000_000,
        active,
    }
}

#[tokio::test]
async fn emit_network_helper_surfaces_warning_and_none_is_silent() {
    use multiview_control::{emit_network_warnings, NetworkMismatchView};
    let h = harness();
    let mut sub = h.engine.subscribe();
    assert_eq!(emit_network_warnings(&h.engine, None, 10), 0);
    assert_eq!(
        emit_network_warnings(&h.engine, Some(NetworkMismatchView), 11),
        1
    );
    assert_eq!(
        warning_ingest_step(&mut sub, h.warnings.as_ref()).await,
        WarningIngestStep::Applied
    );
    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    assert_eq!(arr[0]["code"], "container-no-host-networking");
    assert_eq!(arr[0]["since"], 11);
}

#[tokio::test]
async fn emit_helper_publishes_a_latched_warning_that_ingests_and_surfaces() {
    // The build-site emit seam: the helper publishes the catalog warning through
    // the engine's drop-oldest publisher (inv #10). It rides the Alerts topic,
    // ingests into the store, and surfaces over GET /api/v1/health with the
    // canonical `graphics` / `libvulkan1` remediation. A `None` mismatch (clean
    // host) publishes nothing.
    use multiview_control::{emit_capability_warnings, CompositeMismatchView};

    let h = harness();
    let mut sub = h.engine.subscribe();

    // A clean host: no mismatch → ZERO events published.
    let emitted = emit_capability_warnings(&h.engine, None, 7);
    assert_eq!(emitted, 0, "a clean host must publish no warnings");

    // The mismatch case: a discovered GPU on a software/no-adapter composite.
    let emitted = emit_capability_warnings(
        &h.engine,
        Some(CompositeMismatchView {
            gpu_name: Some("NVIDIA GeForce RTX 4060".to_owned()),
            no_adapter: true,
        }),
        42,
    );
    assert_eq!(emitted, 1, "a mismatch must publish exactly one warning");

    // It rides the stream and ingests.
    let step = warning_ingest_step(&mut sub, h.warnings.as_ref()).await;
    assert_eq!(step, WarningIngestStep::Applied);

    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["code"], "gpu-present-no-vulkan-adapter");
    assert_eq!(arr[0]["since"], 42);
    // The message names the detected GPU and the remediation carries the fix.
    assert!(arr[0]["message"].as_str().unwrap().contains("RTX 4060"));
    assert!(arr[0]["remediation"]
        .as_str()
        .unwrap()
        .contains("NVIDIA_DRIVER_CAPABILITIES"));
    assert!(arr[0]["remediation"]
        .as_str()
        .unwrap()
        .contains("libvulkan1"));
}

#[tokio::test]
async fn health_is_empty_when_clean() {
    // The no-false-positive end state: no warnings raised → the endpoint returns
    // an empty list (the banner renders nothing).
    let h = harness();
    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    assert!(
        arr.as_array().unwrap().is_empty(),
        "clean host: no warnings"
    );
}

#[tokio::test]
async fn raised_warning_flows_through_ingest_and_is_listed_with_remediation() {
    // The end-to-end wiring: an engine health.warning.raised event, drained by the
    // ingest into the SHARED store the router reads, becomes visible over REST
    // carrying the actionable remediation text.
    let h = harness();
    let mut sub = h.engine.subscribe();

    h.engine
        .publish_event(Event::HealthWarningRaised(warning(true)));

    let step = warning_ingest_step(&mut sub, h.warnings.as_ref()).await;
    assert_eq!(step, WarningIngestStep::Applied);

    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let arr = body_json(resp).await;
    let arr = arr.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["code"], "gpu-present-no-vulkan-adapter");
    assert_eq!(arr[0]["severity"], "warning");
    assert_eq!(arr[0]["subsystem"], "compositor");
    assert!(arr[0]["remediation"].as_str().unwrap().contains("graphics"));
    assert_eq!(arr[0]["active"], true);
}

#[tokio::test]
async fn warning_is_latched_emitting_twice_yields_one_active_entry() {
    // Latched / idempotent: the same warning code emitted twice coalesces to ONE
    // active entry (it cannot flap or stack — a build-time fact).
    let h = harness();
    let mut sub = h.engine.subscribe();

    h.engine
        .publish_event(Event::HealthWarningRaised(warning(true)));
    h.engine
        .publish_event(Event::HealthWarningRaised(warning(true)));

    assert_eq!(
        warning_ingest_step(&mut sub, h.warnings.as_ref()).await,
        WarningIngestStep::Applied
    );
    assert_eq!(
        warning_ingest_step(&mut sub, h.warnings.as_ref()).await,
        WarningIngestStep::Applied
    );

    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    assert_eq!(
        arr.as_array().unwrap().len(),
        1,
        "a re-raised latched warning must not stack into a second entry"
    );
}

#[tokio::test]
async fn cleared_warning_drops_out_of_the_active_list() {
    let h = harness();
    let mut sub = h.engine.subscribe();

    h.engine
        .publish_event(Event::HealthWarningRaised(warning(true)));
    h.engine
        .publish_event(Event::HealthWarningCleared(warning(false)));

    assert_eq!(
        warning_ingest_step(&mut sub, h.warnings.as_ref()).await,
        WarningIngestStep::Applied
    );
    assert_eq!(
        warning_ingest_step(&mut sub, h.warnings.as_ref()).await,
        WarningIngestStep::Applied
    );

    // Default list returns only ACTIVE warnings: a cleared one is excluded.
    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    let arr = body_json(resp).await;
    assert!(
        arr.as_array().unwrap().is_empty(),
        "a cleared warning must drop out of the active list"
    );
}

#[tokio::test]
async fn unauthenticated_health_listing_is_401() {
    let h = harness();
    let resp = send(&h.router, get("/api/v1/health", "bogus.token")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let problem = body_json(resp).await;
    assert_eq!(problem["type"], "/problems/unauthenticated");
}

#[tokio::test]
async fn ingest_skips_non_warning_events() {
    let h = harness();
    let mut sub = h.engine.subscribe();
    // A non-warning event the ingest must ignore.
    h.engine.publish_event(Event::Ping);
    assert_eq!(
        warning_ingest_step(&mut sub, h.warnings.as_ref()).await,
        WarningIngestStep::Skipped
    );
}

#[cfg(feature = "openapi")]
#[test]
fn openapi_health_warning_mirror_matches_the_events_serde_shape() {
    // The OpenAPI schema mirror (HealthWarningDoc) must serialise to the SAME JSON
    // shape as the real events HealthWarning it documents, or the published
    // contract would lie. Round-trip a real warning's JSON THROUGH the mirror.
    use multiview_control::openapi_schemas::HealthWarningDoc;

    let events_json = serde_json::to_value(warning(true)).unwrap();
    let doc: HealthWarningDoc = serde_json::from_value(events_json.clone()).unwrap();
    let doc_json = serde_json::to_value(&doc).unwrap();
    assert_eq!(
        events_json, doc_json,
        "the OpenAPI mirror must match the events HealthWarning serde shape"
    );
}

#[tokio::test]
async fn slow_ingest_lags_without_back_pressuring_the_engine() {
    // The chaos property (invariant #10): the engine publishes far more warning
    // events than the ring while ingest never drains. publish_event must remain
    // wait-free; ingest recovers via lagged-skip.
    let h = support::harness();
    let mut sub = h.engine.subscribe();

    for i in 0..2000 {
        let seq = h
            .engine
            .publish_event(Event::HealthWarningRaised(warning(true)));
        assert_eq!(seq, u64::try_from(i + 1).unwrap());
    }

    let step = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        warning_ingest_step(&mut sub, h.warnings.as_ref()),
    )
    .await
    .expect("lagged recovery must not block");
    assert_eq!(step, WarningIngestStep::Lagged);
}

/// #324 review D: the acknowledge must not write the store ahead of the warning
/// ingest. A raise still queued in the engine broadcast when the operator
/// acknowledges would otherwise be applied AFTER a direct "cleared" upsert and
/// re-activate the alarm. Here the ingest is driven by hand: the acknowledge
/// returns only once its own clear (FIFO behind the raise) was applied, and the
/// store then stays cleared.
#[tokio::test]
async fn ack_waits_for_its_clear_behind_a_queued_raise() {
    use multiview_config::{MultiviewConfig, StartMode};
    use multiview_control::boot_model::{
        boot_fallback_marker_path, write_atomic, BootFallbackMarker, BootModel,
    };
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let boot_path = dir.path().join("conspect.toml");
    let doc = "schema_version = 1\n[canvas]\nwidth = 64\nheight = 64\nfps = \"25/1\"\npixel_format = \"nv12\"\nbackground = \"#000000\"\n[canvas.color]\nprofile = \"sdr-bt709-limited\"\n[layout]\nkind = \"absolute\"\n[[sources]]\nid = \"a\"\nkind = \"bars\"\n[[cells]]\nid = \"c\"\nrect = { x = 0.0, y = 0.0, w = 1.0, h = 1.0 }\n[cells.source]\ninput_id = \"a\"\n";
    std::fs::write(&boot_path, doc).unwrap();
    let config = MultiviewConfig::load_from_toml(doc).unwrap();
    let model = Arc::new(BootModel::new(
        boot_path.clone(),
        config.clone(),
        config,
        StartMode::Boot,
        false,
        None,
    ));
    std::fs::create_dir_all(dir.path().join(".multiview")).unwrap();
    let marker = BootFallbackMarker::new("r".into(), None, 1, "m".into());
    write_atomic(
        &boot_fallback_marker_path(&boot_path),
        &serde_json::to_string(&marker).unwrap(),
    )
    .unwrap();
    let h = support::harness_with(|state| state.with_boot_model(model));
    let mut sub = h.engine.subscribe();
    // The engine raised the warning; the ingest has not applied it yet.
    multiview_control::emit_accepted_state_rejected(&h.engine, "raised", 1);
    let ack = tokio::spawn({
        let router = h.router.clone();
        async move {
            send(
                &router,
                support::post_if_match(
                    "/api/v1/health/accepted-state-rejected/ack",
                    support::OPERATOR_TOKEN,
                    None,
                ),
            )
            .await
            .status()
        }
    });
    // Give the acknowledge time to run while the raise is still queued.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // Now the ingest applies the queued raise.
    let applied = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        warning_ingest_step(&mut sub, h.warnings.as_ref()),
    )
    .await
    .expect("the ingest receives the raise");
    assert_eq!(applied, WarningIngestStep::Applied);
    // Whenever the acknowledge has answered, the alarm must be cleared — an
    // acknowledge that wrote the store before the queued raise was applied
    // answers 204 while the raise re-activates it.
    if ack.is_finished() {
        let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
        let body = body_json(resp).await;
        assert!(
            !body.to_string().contains("accepted-state-rejected"),
            "answered 204 but the alarm is active: {body}"
        );
    }
    // Then the acknowledge's own clear, FIFO behind the raise.
    let applied = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        warning_ingest_step(&mut sub, h.warnings.as_ref()),
    )
    .await
    .expect("the ingest receives the clear");
    assert_eq!(applied, WarningIngestStep::Applied);
    assert_eq!(ack.await.unwrap(), StatusCode::NO_CONTENT);
    let resp = send(&h.router, get("/api/v1/health", VIEWER_TOKEN)).await;
    let body = body_json(resp).await;
    assert!(
        !body.to_string().contains("accepted-state-rejected"),
        "{body}"
    );
    assert!(!boot_fallback_marker_path(&boot_path).exists());
}
