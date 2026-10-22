//! `multiview run` cold-start policy (ADR-W024): `[control] start = "resume"`
//! starts from the persisted `active.toml` Running state when it exists,
//! parses, AND validates — and falls back to the boot document with a warning
//! otherwise. The boot file stays the Loaded snapshot and the watch target in
//! BOTH modes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use multiview_cli::boot::resolve_start_config;
use multiview_config::MultiviewConfig;

/// A boot document with the given `[control]` extras spliced in.
fn boot_doc(control_extra: &str) -> String {
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
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[control]
listen = "[::1]:0"
{control_extra}
[[sources]]
id = "in_a"
kind = "solid"
color = "#101418"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/boot-resume.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    )
}

/// Write `boot` to a temp config file (and optionally `active` to the state
/// dir next to it), returning the boot path + the parsed boot config.
fn stage(boot: &str, active: Option<&str>) -> (tempfile::TempDir, PathBuf, MultiviewConfig) {
    let dir = tempfile::tempdir().expect("temp dir");
    let boot_path = dir.path().join("multiview.toml");
    std::fs::write(&boot_path, boot).expect("write boot file");
    if let Some(active) = active {
        let state_dir = dir.path().join(".multiview");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::write(state_dir.join("active.toml"), active).expect("write active.toml");
    }
    let config = MultiviewConfig::load_from_toml(boot).expect("parse boot");
    config.validate().expect("boot validates");
    (dir, boot_path, config)
}

/// The colour of `in_a` in a config, via its serialized body.
fn in_a_color(config: &MultiviewConfig) -> Option<String> {
    config
        .sources
        .iter()
        .find(|s| s.id == "in_a")
        .and_then(|s| {
            serde_json::to_value(s)
                .ok()
                .and_then(|v| v.get("color").and_then(|c| c.as_str().map(str::to_owned)))
        })
}

/// `start = "resume"` with a valid `active.toml` starts Running from it, while
/// Loaded stays the boot snapshot.
#[test]
fn resume_starts_from_a_valid_active_toml() {
    let boot = boot_doc("start = \"resume\"");
    let active = boot.replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(start.resumed, "a valid active.toml must be resumed");
    assert!(start.resume_fallback.is_none());
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#f0f0f0"),
        "Running must be the active.toml document"
    );
    assert_eq!(
        in_a_color(&start.loaded).as_deref(),
        Some("#101418"),
        "Loaded must stay the boot snapshot"
    );
}

/// A corrupt `active.toml` falls back to the boot document with a surfaced
/// reason.
#[test]
fn resume_falls_back_to_boot_on_a_corrupt_active() {
    let boot = boot_doc("start = \"resume\"");
    let (_dir, boot_path, boot_config) = stage(&boot, Some("this is [not the schema"));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(!start.resumed, "a corrupt active.toml must not resume");
    let reason = start
        .resume_fallback
        .expect("the fallback reason is surfaced");
    assert!(
        reason.contains("parse") || reason.contains("TOML") || reason.contains("read"),
        "the reason should be actionable, got: {reason}"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#101418"),
        "Running must fall back to the boot document"
    );
}

/// A missing `active.toml` under `start = "resume"` also falls back (warned).
#[test]
fn resume_falls_back_to_boot_when_active_is_missing() {
    let boot = boot_doc("start = \"resume\"");
    let (_dir, boot_path, boot_config) = stage(&boot, None);

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(!start.resumed);
    assert!(start.resume_fallback.is_some());
    assert_eq!(in_a_color(&start.running).as_deref(), Some("#101418"));
}

/// Review M1 — resume staleness: the storeless restart-only sections
/// (`control`, `placement`, `walls`, `routing`, `schema_version`) must be
/// spliced from the BOOT file into the resumed Running document — a boot-file
/// `[control] listen` edit must take effect on the restart the operator
/// performed, while the live sections still resume from `active.toml`.
/// (ADR-W024 round 6: `salvos`/`tally_profiles` are store-backed running state
/// resumed from `active.toml`, no longer spliced — covered by the control-plane
/// `boot_config_model` suite.)
#[test]
fn resume_splices_storeless_sections_from_the_boot_file() {
    // The previous run persisted active.toml under the OLD listen; the
    // operator then edited the BOOT file's [control] listen and restarted.
    let boot = boot_doc("start = \"resume\"").replace("[::1]:0", "[::1]:9099");
    let active = boot_doc("start = \"resume\"").replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(start.resumed, "the valid active.toml must still resume");
    assert_eq!(
        start
            .running
            .control
            .as_ref()
            .map(|control| control.listen.clone())
            .as_deref(),
        Some("[::1]:9099"),
        "the restart-only [control] section must come from the BOOT file, not the stale active.toml"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#f0f0f0"),
        "the live sections still resume from active.toml"
    );
    assert_eq!(
        start.running.schema_version, start.loaded.schema_version,
        "schema_version follows the boot document"
    );
}

/// Review M1 fallback branch: when the splice produces a document that no
/// longer validates, the run falls back to the boot document with the reason
/// surfaced — never starts from an invalid combination.
///
/// ADR-W024 round 6: `salvos`/`tally_profiles` are NO LONGER spliced (they are
/// store-backed running state that resumes from `active.toml`), so the invalid
/// combination is now driven by a STILL-spliced section — the `[routing]` block.
/// Boot is a two-cell grid (`cell_a` ← `in_a`, `cell_b` ← `in_b`) with a routing
/// video crosspoint `cell_b` ← `in_b`; the persisted Running state
/// (`active.toml`) dropped `cell_b` + `in_b` live (a valid single-cell document
/// with no routing). Layout/cells resume from `active.toml` (live-sheddable, not
/// spliced) so the resumed document has only `cell_a`/`in_a`, but `routing`
/// SPLICES from boot and now references the dropped `cell_b`/`in_b` → the splice
/// no longer validates → fallback to boot.
#[test]
fn resume_falls_back_when_the_splice_does_not_validate() {
    let boot = r##"schema_version = 1
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
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[control]
listen = "[::1]:0"
start = "resume"
[[sources]]
id = "in_a"
kind = "solid"
color = "#101418"
[[sources]]
id = "in_b"
kind = "bars"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[routing.video]]
cell = "cell_b"
source = { input_id = "in_b", kind = { kind = "video" } }
[[outputs]]
kind = "hls"
path = "/tmp/boot-resume.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    .to_owned();
    // The previous run removed cell_b + in_b live; its persisted Running state
    // is a valid single-cell document with neither and no routing. The spliced
    // boot `[routing]` then references the missing cell_b/in_b.
    let active = r##"schema_version = 1
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
[control]
listen = "[::1]:0"
start = "resume"
[[sources]]
id = "in_a"
kind = "solid"
color = "#f0f0f0"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/boot-resume.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    let (_dir, boot_path, boot_config) = stage(&boot, Some(active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(
        !start.resumed,
        "a splice that does not validate must not resume"
    );
    let reason = start
        .resume_fallback
        .expect("the fallback reason is surfaced");
    assert!(
        reason.contains("validate"),
        "the reason should name the validation failure, got: {reason}"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#101418"),
        "Running must fall back to the boot document"
    );
}

/// ADR-W024 round 6 (F1): `salvos` and `tally_profiles` are store-backed running
/// state and MUST resume FROM `active.toml`, never be spliced back to the boot
/// file's copy — a runtime salvo/tally edit (persisted to `active.toml`) must
/// survive a `resume` restart. Boot declares salvo `s1` + tally profile `tp1`;
/// the operator edited both live (a renamed salvo, a profile that drops a
/// binding), so `active.toml` carries the EDITED definitions. After resume the
/// Running document must reflect `active.toml`'s definitions, not boot's.
#[test]
fn resume_keeps_runtime_salvo_and_tally_edits_from_active() {
    // Boot: salvo "s1" (display "Boot name") + tally profile "tp1" with two
    // index bindings. `[control] start = "resume"`.
    let boot = format!(
        "{}[[salvos]]\nid = \"s1\"\ndisplay_name = \"Boot name\"\n\
         [[salvos.tally]]\ncell = \"cell_a\"\ncolor = \"Red\"\n\
         [[tally_profiles]]\nid = \"tp1\"\n\
         [[tally_profiles.index_cells]]\nindex = 0\ncell = \"cell_a\"\n",
        boot_doc("start = \"resume\"")
    );
    // active.toml = the persisted Running state after live edits: the salvo was
    // renamed, and the tally profile kept only index 0 (a real machine-written
    // valid document on its own).
    let active = format!(
        "{}[[salvos]]\nid = \"s1\"\ndisplay_name = \"Edited live\"\n\
         [[salvos.tally]]\ncell = \"cell_a\"\ncolor = \"Green\"\n\
         [[tally_profiles]]\nid = \"tp1\"\n\
         [[tally_profiles.index_cells]]\nindex = 0\ncell = \"cell_a\"\n",
        boot_doc("start = \"resume\"").replace("#101418", "#f0f0f0")
    );
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(start.resumed, "the valid active.toml must resume");
    let salvo = start
        .running
        .salvos
        .iter()
        .find(|s| s.id == "s1")
        .expect("the resumed Running state carries the salvo");
    assert_eq!(
        salvo.display_name.as_deref(),
        Some("Edited live"),
        "the runtime salvo edit must resume from active.toml, NOT be spliced back to the boot copy"
    );
    // The live sections still resume from active.toml (the recolor too).
    assert_eq!(in_a_color(&start.running).as_deref(), Some("#f0f0f0"));
}

/// The default policy is `boot`: an existing `active.toml` is IGNORED unless
/// the boot file opts into resume.
#[test]
fn the_default_boot_policy_ignores_an_existing_active() {
    let boot = boot_doc("");
    let active = boot.replace("#101418", "#f0f0f0");
    let (_dir, boot_path, boot_config) = stage(&boot, Some(&active));

    let start = resolve_start_config(boot_config, boot.clone(), &boot_path);
    assert!(!start.resumed, "start = boot must never resume");
    assert!(
        start.resume_fallback.is_none(),
        "no fallback warning either"
    );
    assert_eq!(
        in_a_color(&start.running).as_deref(),
        Some("#101418"),
        "Running must be the boot document"
    );
}

/// MINOR-D (ADR-W024 §6): when the config is reached through a SYMLINK, the
/// boot model's path must be CANONICALIZED to the real target, so a later
/// `promote` rewrites the real file (the atomic temp + rename would otherwise
/// replace the symlink itself). `to_boot_model` resolves the link at startup.
#[cfg(unix)]
#[test]
fn to_boot_model_canonicalizes_a_symlinked_config() {
    let dir = tempfile::tempdir().expect("temp dir");
    let real = dir.path().join("real-multiview.toml");
    let link = dir.path().join("multiview.toml");
    let boot = boot_doc("");
    std::fs::write(&real, &boot).expect("write real config");
    std::os::unix::fs::symlink(&real, &link).expect("create symlink");

    let config = MultiviewConfig::load_from_toml(&boot).expect("parse");
    let start = resolve_start_config(config, boot, &link);
    let model = start.to_boot_model(&link);

    let canonical_real = std::fs::canonicalize(&real).expect("canonicalize real");
    assert_eq!(
        model.boot_path(),
        canonical_real.as_path(),
        "the boot model must resolve the symlink to the real file so promote writes through it"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("stat link")
            .file_type()
            .is_symlink(),
        "the symlink itself is untouched"
    );
}
