#![cfg(unix)]
//! Reproduce HTTP's lost receipt after durable correlated service admission.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod support;
use multiview_control::{
    web_capture::{CaptureBody, Provider},
    web_capture_store::{digest_hex, Ledger, MemoryTerms, Reserve},
    Principal, Role,
};
use multiview_webengine::{
    artifact::{ArtifactStore, Limits},
    capture::{Background, Capture, Tools, WindyView},
    service::{Config, Request, Service, Submitted},
};
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
#[test]
#[allow(clippy::too_many_lines)]
fn recovered_http_pending_receipt_binds_existing_job_without_another_producer() {
    recovery_case(false);
}
#[test]
fn pruned_correlation_never_turns_a_pending_http_receipt_into_fresh_admission() {
    recovery_case(true);
}
#[allow(clippy::too_many_lines)]
fn recovery_case(prune: bool) {
    let root = tempfile::tempdir().unwrap();
    let tool = root.path().join("node");
    let marker = root.path().join("runs");
    std::fs::write(&tool,format!("#!/bin/sh\ncat >/dev/null\nprintf x >> '{}'\nprintf media\nprintf '%s\\n' '{{\"event\":\"capture_complete\",\"frames\":2,\"steps_kept\":2}}' >&2\n",marker.display())).unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let store_dir = root.path().join("store");
    std::fs::create_dir(&store_dir).unwrap();
    let service = Arc::new(
        Service::start_durable(
            Tools {
                node: tool,
                browser: "/bin/sh".into(),
                ffmpeg: "/bin/sh".into(),
                gl: multiview_webengine::capture::BrowserGl::Default,
            },
            ArtifactStore::open(
                &store_dir,
                Limits {
                    bytes: 128 * 1024 * 1024,
                    count: 64,
                },
            )
            .unwrap(),
            Config {
                queue_capacity: 2,
                history: 8,
                max_fills: 32,
            },
            &root.path().join("jobs"),
        )
        .unwrap(),
    );
    let actor = Principal {
        key_id: "operator-key".into(),
        role: Role::Operator,
        scoped_object_ids: None,
        scoped_output_ids: None,
        scoped_discovery_domains: None,
    };
    let body:CaptureBody=serde_json::from_value(serde_json::json!({"fill_id":"radar","view":{"lat":-33.946,"lon":150.37,"zoom":10,"overlay":"radar","width":1280,"height":720,"fps":2,"frames":2,"public":true},"terms":{"accepted":true,"version":"windy-2023-09-01"}})).unwrap();
    let digest = digest_hex(&serde_json::to_vec(&body).unwrap());
    let key = "lost-receipt";
    let token = digest_hex(
        &serde_json::to_vec(&serde_json::json!([
            actor.key_id,
            "submit",
            digest_hex(key.as_bytes()),
            digest
        ]))
        .unwrap(),
    );
    let ledger_path = root.path().join("keys");
    {
        let ledger = Ledger::durable(&ledger_path).unwrap();
        assert_eq!(
            ledger.reserve(&actor.key_id, "submit", key, &digest),
            Ok(Reserve::Fresh)
        );
    }
    let id = match service
        .submit_correlated(
            Request {
                owner: actor.key_id.clone(),
                fill: "radar".into(),
                capture: Capture {
                    width: 1280,
                    height: 720,
                    fps: 2,
                    frames: 2,
                    document: String::new(),
                    background: Background::Opaque,
                    windy: Some(WindyView {
                        lat: -33.946,
                        lon: 150.37,
                        zoom: 10,
                        overlay: "radar".into(),
                        terms_accepted: true,
                        require_premium: false,
                        credentials: None,
                    }),
                    capture: multiview_webengine::capture::CaptureOptions::default(),
                },
                maximum_bytes: 64 * 1024 * 1024,
                timeout: Duration::from_secs(120),
            },
            &token,
        )
        .unwrap()
    {
        Submitted::Accepted(id) | Submitted::Coalesced(id) => id,
    };
    // No Ledger.complete: model a killed HTTP worker after Service accepted.
    if prune {
        let finish = |job| {
            let guard = std::time::Instant::now() + Duration::from_secs(10);
            while matches!(
                service.status(&actor.key_id, job).unwrap().state,
                multiview_webengine::service::State::Queued
                    | multiview_webengine::service::State::Running
            ) {
                assert!(std::time::Instant::now() < guard);
                std::thread::yield_now();
            }
        };
        finish(id);
        for n in 0..12 {
            let request = Request {
                owner: actor.key_id.clone(),
                fill: format!("other-{n}"),
                capture: Capture {
                    width: 1280,
                    height: 720,
                    fps: 2,
                    frames: 2,
                    document: "trusted fixture".into(),
                    background: Background::Opaque,
                    windy: None,
                    capture: multiview_webengine::capture::CaptureOptions::default(),
                },
                maximum_bytes: 1024,
                timeout: Duration::from_secs(120),
            };
            let job = match service.submit(request).unwrap() {
                Submitted::Accepted(id) | Submitted::Coalesced(id) => id,
            };
            finish(job);
        }
        assert!(service
            .lookup_correlated(&actor.key_id, "radar", &token)
            .is_none());
    }
    let runs_before_retry = std::fs::read(&marker).unwrap_or_default().len();
    let recovered = Arc::new(Ledger::durable(&ledger_path).unwrap());
    assert!(recovered.outcome_uncertain(&actor.key_id, "submit", key));
    let provider =
        Provider::configured(service.clone(), Arc::new(MemoryTerms::default()), recovered);
    if prune {
        assert!(matches!(
            provider.submit(&actor, key, &body),
            Err(multiview_control::ControlError::CaptureOutcomeUncertain)
        ));
        assert_eq!(
            std::fs::read(&marker).unwrap_or_default().len(),
            runs_before_retry
        );
    } else {
        let result = provider.submit(&actor, key, &body).unwrap();
        assert_eq!(result.job_id, id.raw());
        assert!(!result.fresh);
    }
    service.shutdown();
    assert!(
        std::fs::read(&marker).map_or(true, |runs| runs.len()
            <= if prune { runs_before_retry } else { 1 }),
        "a recovered HTTP receipt must not launch another producer"
    );
}
