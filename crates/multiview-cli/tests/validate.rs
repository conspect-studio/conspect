//! Integration tests for the `validate` subcommand.
//!
//! `validate` must succeed on every shipped example config and must fail with a
//! clear, specific message on a deliberately broken config. These assert the
//! *report* model directly (status + rendered human text), which is exactly
//! what the binary prints.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs;
use std::path::{Path, PathBuf};

use multiview_cli::validate::validate_config;

/// Absolute path to the workspace `examples/` directory, resolved from this
/// crate's manifest dir so the test is cwd-independent.
fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
}

#[test]
fn every_example_config_validates() {
    let dir = examples_dir();
    let mut checked = 0_usize;
    for entry in fs::read_dir(&dir).expect("examples dir must exist") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let report = validate_config(&path)
            .unwrap_or_else(|e| panic!("validate_config({}) errored: {e}", path.display()));
        assert!(
            report.is_ok(),
            "example {} should validate, but: {report:?}",
            path.display()
        );
        // The rendered report must mention the file and an OK marker.
        let rendered = report.render();
        assert!(
            rendered.contains("OK") || rendered.contains("ok"),
            "rendered ok-report should signal success: {rendered}"
        );
        checked += 1;
    }
    assert!(
        checked >= 4,
        "expected to validate several examples, got {checked}"
    );
}

#[test]
fn broken_config_fails_with_a_clear_message() {
    // A config that parses but references an undeclared source id — a semantic
    // (not syntactic) violation that `MultiviewConfig::validate` must catch.
    let toml = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "absolute"

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
rect = { x = 0.0, y = 0.0, w = 0.5, h = 0.5 }
[cells.source]
input_id = "does_not_exist"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;

    let dir = tempdir();
    let path = dir.join("broken.toml");
    fs::write(&path, toml).expect("write temp config");

    let report = validate_config(&path)
        .expect("validate_config returns Ok(report) even when the document is invalid");
    assert!(!report.is_ok(), "broken config must NOT validate");

    let rendered = report.render();
    assert!(
        rendered.contains("does_not_exist"),
        "the failure report must name the offending unknown source id; got:\n{rendered}"
    );
    assert!(
        rendered.contains("FAIL") || rendered.contains("error") || rendered.contains("invalid"),
        "the failure report must clearly signal failure; got:\n{rendered}"
    );
}

#[test]
fn clock_advisory_warnings_are_surfaced_in_the_validate_report() {
    // A clock that sets BOTH `timezone` (IANA, authoritative) and a non-zero
    // `tz_offset_minutes` is legal but redundant: `Source::clock_warnings()`
    // reports the ignored offset. That advisory must reach the operator — the
    // rendered validate report carries it as a WARN line (the config still
    // validates OK).
    let toml = r##"
schema_version = 1

[canvas]
width = 1280
height = 720
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
id = "clock_syd"
kind = "clock"
timezone = "Australia/Sydney"
tz_offset_minutes = 600

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "clock_syd"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;

    let dir = tempdir();
    let path = dir.join("clock-both.toml");
    fs::write(&path, toml).expect("write temp config");

    let report = validate_config(&path).expect("validate_config returns a report");
    assert!(
        report.is_ok(),
        "a both-present clock is legal (timezone wins); got: {report:?}"
    );
    let rendered = report.render();
    assert!(
        rendered.contains("WARN"),
        "the report must carry the advisory as a WARN line; got:\n{rendered}"
    );
    assert!(
        rendered.contains("tz_offset_minutes") && rendered.contains("clock_syd"),
        "the warning must name the source and the ignored field; got:\n{rendered}"
    );
}

#[test]
fn clock_without_conflicts_renders_no_warn_lines() {
    // The inverse guard: a clean clock (timezone only) must NOT emit WARN lines,
    // so the advisory path cannot fire spuriously.
    let toml = r##"
schema_version = 1

[canvas]
width = 1280
height = 720
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
id = "clock_syd"
kind = "clock"
timezone = "Australia/Sydney"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "clock_syd"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;

    let dir = tempdir();
    let path = dir.join("clock-clean.toml");
    fs::write(&path, toml).expect("write temp config");

    let report = validate_config(&path).expect("validate_config returns a report");
    assert!(
        report.is_ok(),
        "a timezone-only clock validates: {report:?}"
    );
    let rendered = report.render();
    assert!(
        !rendered.contains("WARN"),
        "no advisory applies, so no WARN line; got:\n{rendered}"
    );
}

#[test]
fn malformed_toml_fails_with_a_parse_message() {
    let dir = tempdir();
    let path = dir.join("garbage.toml");
    fs::write(&path, "this is = = not toml [[[").expect("write temp config");

    let report = validate_config(&path).expect("a parse failure is reported, not returned as Err");
    assert!(!report.is_ok());
    let rendered = report.render();
    assert!(
        rendered.to_lowercase().contains("parse") || rendered.to_lowercase().contains("toml"),
        "a malformed-TOML report should mention parsing/TOML; got:\n{rendered}"
    );
}

#[test]
fn missing_file_is_reported_clearly() {
    let path = PathBuf::from("/no/such/multiview/config/file.toml");
    let report = validate_config(&path).expect("a missing file is reported, not a panic");
    assert!(!report.is_ok());
    assert!(
        report.render().to_lowercase().contains("read")
            || report.render().to_lowercase().contains("no such")
            || report.render().to_lowercase().contains("not found")
            || report.render().to_lowercase().contains("file"),
        "missing-file report should mention the read failure; got:\n{}",
        report.render()
    );
}

/// A unique temporary directory under the OS temp dir, created for the test.
fn tempdir() -> PathBuf {
    let base = std::env::temp_dir();
    let unique = format!(
        "multiview-cli-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let dir = base.join(unique);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}
