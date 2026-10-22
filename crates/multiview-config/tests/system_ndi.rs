//! `[system.ndi] accept_license` config surface (ADR-0008 §7.5): the single,
//! audited operator acceptance the NDI license gate (ingest **and** output) reads.
//!
//! Pure serde + validation — no engine, no NDI feature. Load-bearing properties:
//! the acceptance is **exported as a flag, never a secret** (it round-trips
//! plainly, is not redacted); `accept_license = true` **requires** the audit
//! fields (who/when) and is rejected at validate time otherwise (the same who/when
//! invariant `NdiLicense::accept` enforces, surfaced at config-load); declining
//! (`accept_license = false`) needs no audit.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, NdiSystemConfig, SystemConfig};

/// A minimal, valid 2x2 grid document used as the base for `[system.ndi]` mutation
/// (mirrors `tests/validation.rs`).
const BASE: &str = r##"
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
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
gap = 8
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[cells]]
id = "cell_b"
area = "b"
fit = "contain"
[cells.source]
input_id = "in_b"

[[cells]]
id = "cell_c"
area = "c"
fit = "contain"
[cells.source]
input_id = "in_c"

[[cells]]
id = "cell_d"
area = "d"
fit = "contain"
[cells.source]
input_id = "in_d"

[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;

#[test]
fn system_ndi_round_trips_as_a_flag_never_a_secret() {
    let sys: SystemConfig = toml::from_str(
        r#"
[ndi]
accept_license = true
accepted_by = "operator@example"
accepted_at = "2026-06-06T00:00:00Z"
"#,
    )
    .expect("a [system.ndi] block deserializes");

    let ndi = sys.ndi.as_ref().expect("ndi sub-table present");
    assert!(ndi.accept_license);
    assert_eq!(ndi.accepted_by.as_deref(), Some("operator@example"));
    assert_eq!(ndi.accepted_at.as_deref(), Some("2026-06-06T00:00:00Z"));

    // Exported as a flag, never a secret: the acceptance + audit serialize plainly
    // (nothing is redacted / behind a secret_ref), so it round-trips visibly.
    let dumped = toml::to_string(&sys).expect("serializes");
    assert!(
        dumped.contains("accept_license = true"),
        "the acceptance flag must export plainly, got:\n{dumped}"
    );
    assert!(
        dumped.contains("operator@example"),
        "the audit principal must export plainly (not redacted), got:\n{dumped}"
    );
}

#[test]
fn accept_with_complete_audit_validates() {
    let doc = format!(
        "{BASE}\n[system.ndi]\naccept_license = true\naccepted_by = \"operator@example\"\n\
         accepted_at = \"2026-06-06T00:00:00Z\"\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("loads");
    cfg.validate()
        .expect("an accepted + fully-audited [system.ndi] must validate");
    let ndi = cfg
        .system
        .as_ref()
        .and_then(|s| s.ndi.as_ref())
        .expect("system.ndi present");
    assert!(ndi.accept_license);
}

#[test]
fn accept_without_audit_is_rejected_at_validation() {
    let doc = format!("{BASE}\n[system.ndi]\naccept_license = true\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("loads (shape is valid)");
    let err = cfg
        .validate()
        .expect_err("accept_license=true with no audit (who/when) must be rejected");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("ndi") && (msg.contains("accept") || msg.contains("audit")),
        "the error must name the NDI acceptance audit gap, got: {err}"
    );
}

#[test]
fn declining_needs_no_audit() {
    // Declining is the default-safe state: `accept_license = false` with no audit
    // fields validates (you can decline without an audit record).
    let doc = format!("{BASE}\n[system.ndi]\naccept_license = false\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("loads");
    cfg.validate()
        .expect("declining the NDI license needs no audit");
    let ndi = cfg
        .system
        .as_ref()
        .and_then(|s| s.ndi.as_ref())
        .expect("system.ndi present");
    assert!(!ndi.accept_license);
}

#[test]
fn ndi_system_config_can_be_constructed_for_the_gate() {
    // The struct the binary builds to feed `NdiLicense::from_setting` at the ingest
    // construction point.
    let ndi = NdiSystemConfig {
        accept_license: true,
        accepted_by: Some("ops".to_owned()),
        accepted_at: Some("2026-06-06T00:00:00Z".to_owned()),
    };
    assert!(ndi.accept_license);
}
