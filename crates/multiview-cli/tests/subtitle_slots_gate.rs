//! Subtitle-slot build-gate tests (#508): an output that declares subtitle
//! slots must FAIL the startup/validate check, clearly, in a build without the
//! `overlay` feature. Without `overlay` no caption source is wired, so the
//! slot's `WebVTT` rendition would be published with every segment empty (the
//! `0 cues` of the local cast proof in #508). With the feature the same check
//! passes. A config without subtitle slots always passes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_cli::outputs::ensure_subtitle_slots_supported;
use multiview_config::MultiviewConfig;

/// A minimal valid document with one cast output; `slots_toml` is appended
/// after it verbatim.
fn config_with_cast(slots_toml: &str) -> MultiviewConfig {
    let doc = format!(
        r##"
schema_version = 1

[canvas]
width = 640
height = 360
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "preset"
preset = "2x2"

[[sources]]
id = "in_a"
kind = "bars"

[[cells]]
id = "cell_a"
rect = {{ x = 0.0, y = 0.0, w = 1.0, h = 1.0 }}
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "cast"
id = "cast-1"
{slots_toml}
"##
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("document parses");
    cfg.validate().expect("document validates");
    cfg
}

const SUBTITLE_SLOT: &str = r#"
[[outputs.slots.subtitle]]
name = "English"
language = "en"
source = "in_a"
"#;

#[cfg(not(feature = "overlay"))]
#[test]
fn subtitle_slots_fail_the_gate_without_overlay() {
    let cfg = config_with_cast(SUBTITLE_SLOT);
    let err = ensure_subtitle_slots_supported(&cfg.outputs)
        .expect_err("subtitle slots must be refused in a non-overlay build");
    assert!(err.contains("overlay"), "names the feature: {err}");
    assert!(err.contains("cast-1"), "names the output: {err}");
    assert!(err.contains("no cues"), "says what would happen: {err}");
}

#[cfg(feature = "overlay")]
#[test]
fn subtitle_slots_pass_the_gate_with_overlay() {
    let cfg = config_with_cast(SUBTITLE_SLOT);
    ensure_subtitle_slots_supported(&cfg.outputs).expect("overlay build carries subtitle slots");
}

#[test]
fn outputs_without_subtitle_slots_always_pass() {
    let cfg = config_with_cast("");
    ensure_subtitle_slots_supported(&cfg.outputs).expect("no subtitle slots => always fine");
    let cfg = config_with_cast("[[outputs.slots.audio]]\nlanguage = \"en\"\n");
    ensure_subtitle_slots_supported(&cfg.outputs).expect("audio slots only => always fine");
}

// ---- the start paths (review r1 of PR #514): every state a start can build
// (boot file, accepted state, last-good fallback) passes the same gate.

mod start_paths {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use multiview_cli::boot::FailureClass;
    use multiview_cli::boot::{
        build_with_fallback, resolve_start_config, BuildFailure, RetryPolicy,
    };
    use multiview_cli::last_good::spawn_recorder;
    use multiview_config::{MultiviewConfig, StartMode};
    use multiview_control::{boot_model::BootModel, command_bus, AppState, EngineStateSnapshot};
    use multiview_engine::{EnginePublisher, StopSignal};
    use multiview_events::Event;

    const REFUSAL: &str = "subtitle slots require the overlay build";

    fn boot_text(slots: bool) -> String {
        let slot = if slots { super::SUBTITLE_SLOT } else { "" };
        format!(
            r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "absolute"
[[sources]]
id = "in_a"
kind = "bars"
[[cells]]
id = "pgm"
rect = {{ x = 0.0, y = 0.0, w = 1.0, h = 1.0 }}
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "cast"
id = "cast-1"
{slot}
"##
        )
    }

    fn has_slots(config: &MultiviewConfig) -> bool {
        config
            .outputs
            .iter()
            .any(|o| o.slots().is_some_and(|s| !s.subtitle.is_empty()))
    }

    /// Write `boot` as the boot file and commit it as the accepted state,
    /// the way the control plane does (as `tests/boot_fallback.rs`).
    async fn install(dir: &Path, boot: &str) -> (PathBuf, MultiviewConfig) {
        let path = dir.join("conspect.toml");
        std::fs::write(&path, boot).unwrap();
        let config = MultiviewConfig::load_from_toml(boot).unwrap();
        let (sender, _rx) = command_bus(8);
        let state = AppState::new(
            Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8)),
            sender,
            Arc::new(multiview_control::InMemoryRepository::new()),
            Arc::new(multiview_control::ApiKeyStore::new(b"slots-gate".to_vec())),
        )
        .with_seeded_resources(multiview_control::seed_resources(&config).unwrap())
        .with_base_document(serde_json::to_value(&config).unwrap())
        .with_boot_model(Arc::new(BootModel::new(
            path.clone(),
            config.clone(),
            config.clone(),
            StartMode::Boot,
            false,
            None,
        )));
        let layouts = multiview_control::boot_model::layout_library(&state).unwrap();
        Box::pin(multiview_control::boot_model::commit_desired(
            &state,
            config.clone(),
            layouts,
        ))
        .await
        .unwrap();
        (path, config)
    }

    /// Record the current accepted state as last-good, as a started run does.
    async fn mark_started(path: &Path) {
        let accepted = multiview_control::boot_model::load_desired(path)
            .unwrap()
            .unwrap();
        let ticks = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&ticks);
        let task = spawn_recorder(
            path.to_path_buf(),
            accepted,
            move || observed.load(Ordering::Acquire),
            StopSignal::new(),
        );
        ticks.fetch_add(1, Ordering::AcqRel);
        task.await.unwrap();
    }

    /// A slot-free accepted state that fails a STATE-class build falls back to
    /// a last-good copy WITH subtitle slots: a build without `overlay` must
    /// refuse that candidate too, never start it with empty renditions.
    #[tokio::test]
    async fn the_last_good_fallback_candidate_is_gated() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = Box::pin(install(dir.path(), &boot_text(true))).await;
        mark_started(&path).await;
        let (path, boot) = Box::pin(install(dir.path(), &boot_text(false))).await;
        let start = resolve_start_config(boot, boot_text(false), &path).unwrap();
        assert!(
            !has_slots(&start.running),
            "starts from the slot-free state"
        );
        let retry = RetryPolicy::new(vec![]);
        let built = build_with_fallback(start, &path, &retry, |config| {
            if has_slots(config) {
                Ok(())
            } else {
                Err(BuildFailure::new(
                    FailureClass::State,
                    "the accepted state does not build",
                ))
            }
        });
        if cfg!(feature = "overlay") {
            let (start, ()) = built.expect("falls back to last-good");
            assert!(has_slots(&start.running), "started from last-good");
        } else {
            let err = built.expect_err("the last-good candidate is refused");
            assert!(err.contains(REFUSAL), "{err}");
            assert!(err.contains("last-good"), "{err}");
        }
    }

    /// The accepted state a start would build is gated directly.
    #[tokio::test]
    async fn the_accepted_state_is_gated_before_any_build() {
        let dir = tempfile::tempdir().unwrap();
        let (path, boot) = Box::pin(install(dir.path(), &boot_text(true))).await;
        let start = resolve_start_config(boot, boot_text(true), &path).unwrap();
        let built = build_with_fallback(start, &path, &RetryPolicy::new(vec![]), |_| Ok(()));
        if cfg!(feature = "overlay") {
            built.expect("an overlay build starts it");
        } else {
            let err = built.expect_err("refused");
            assert!(err.contains(REFUSAL), "{err}");
        }
    }

    #[cfg(not(feature = "overlay"))]
    fn binary(args: &[&str], path: &Path) -> (bool, String) {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_multiview"))
            .args(args)
            .arg(path)
            .env("MULTIVIEW_CONTROL_TOKEN", "test-secret")
            .output()
            .expect("the multiview binary runs");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    /// End to end through the real binary: `run` and `validate` refuse a
    /// subtitle-slot config in a build without `overlay`, naming the reason;
    /// `validate` also refuses an accepted state with slots under a slot-free
    /// boot file.
    #[cfg(not(feature = "overlay"))]
    #[tokio::test]
    async fn the_binary_refuses_subtitle_slots_at_run_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conspect.toml");
        std::fs::write(&path, boot_text(true)).unwrap();
        let (ok, text) = binary(&["run", "--software", "--ticks", "2"], &path);
        assert!(!ok && text.contains(REFUSAL), "{text}");
        let (ok, text) = binary(&["validate"], &path);
        assert!(!ok && text.contains(REFUSAL), "{text}");

        let dir = tempfile::tempdir().unwrap();
        let (path, _) = Box::pin(install(dir.path(), &boot_text(true))).await;
        std::fs::write(&path, boot_text(false)).unwrap();
        let (ok, text) = binary(&["validate"], &path);
        assert!(
            !ok && text.contains(REFUSAL),
            "accepted state gated: {text}"
        );
        let (ok, text) = binary(&["run", "--software", "--ticks", "2"], &path);
        assert!(!ok && text.contains(REFUSAL), "{text}");
    }
}
