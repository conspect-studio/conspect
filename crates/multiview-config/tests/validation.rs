//! Failure-mode tests: every semantic invariant `validate()` enforces must
//! reject a document that violates it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::MultiviewConfig;

/// A minimal, valid 2x2-ish grid document used as the base for mutation.
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
fn base_document_is_valid() {
    let cfg = MultiviewConfig::load_from_toml(BASE).unwrap();
    cfg.validate().expect("base document should validate");
}

#[test]
fn dangling_cell_source_input_id_is_rejected() {
    let bad = BASE.replace(r#"input_id = "in_d""#, r#"input_id = "in_NOPE""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).unwrap();
    let err = cfg.validate().expect_err("dangling input_id must fail");
    assert!(
        err.to_string().contains("in_NOPE"),
        "error should name the dangling id, got: {err}"
    );
}

#[test]
fn unknown_cell_area_is_rejected() {
    let bad = BASE.replace(r#"area = "d""#, r#"area = "ZZZ""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).unwrap();
    let err = cfg.validate().expect_err("unknown area must fail");
    assert!(
        err.to_string().contains("ZZZ"),
        "error should name the unknown area, got: {err}"
    );
}

#[test]
fn duplicate_source_id_is_rejected() {
    let bad = BASE.replace(r#"id = "in_b""#, r#"id = "in_a""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).unwrap();
    let err = cfg.validate().expect_err("duplicate source id must fail");
    assert!(
        err.to_string().contains("in_a"),
        "error should name the duplicate id, got: {err}"
    );
}

#[test]
fn duplicate_cell_id_is_rejected() {
    let bad = BASE.replace(r#"id = "cell_b""#, r#"id = "cell_a""#);
    let cfg = MultiviewConfig::load_from_toml(&bad).unwrap();
    let err = cfg.validate().expect_err("duplicate cell id must fail");
    assert!(
        err.to_string().contains("cell_a"),
        "error should name the duplicate cell id, got: {err}"
    );
}

#[test]
fn float_fps_is_rejected_at_parse_time() {
    // A bare float must NOT deserialize as a rational fps string (invariant #3).
    let bad = BASE.replace(r#"fps = "30000/1001""#, "fps = 29.97");
    assert!(
        MultiviewConfig::load_from_toml(&bad).is_err(),
        "float fps must fail to parse"
    );
}

#[test]
fn absent_control_section_is_none_and_valid() {
    let cfg = MultiviewConfig::load_from_toml(BASE).unwrap();
    cfg.validate().expect("base document should validate");
    assert!(
        cfg.control.is_none(),
        "no [control] section ⇒ None (today's headless behaviour)"
    );
}

#[test]
fn control_listener_valid_addr_is_accepted() {
    // IPv6-first (operator directive): the recommended listener is the IPv6
    // wildcard `[::]:8080` (dual-stacks on Linux); it must parse + round-trip.
    let with_control = format!("{BASE}\n[control]\nlisten = \"[::]:8080\"\n");
    let cfg = MultiviewConfig::load_from_toml(&with_control).unwrap();
    cfg.validate()
        .expect("a parseable IPv6 control.listen should validate");
    assert_eq!(
        cfg.control.as_ref().map(|c| c.listen.as_str()),
        Some("[::]:8080"),
        "the control listener address should round-trip from TOML"
    );

    // The IPv6 loopback form also validates.
    let loopback = format!("{BASE}\n[control]\nlisten = \"[::1]:8080\"\n");
    MultiviewConfig::load_from_toml(&loopback)
        .unwrap()
        .validate()
        .expect("an IPv6 loopback control.listen should validate");
}

#[test]
fn control_listener_user_supplied_ipv4_addr_still_accepted() {
    // We are IPv6-first, but a user who *explicitly* supplies an IPv4 listener is
    // never denied (directive: "if IPv4 works, great"); it just is not a default.
    let with_v4 = format!("{BASE}\n[control]\nlisten = \"127.0.0.1:8080\"\n");
    let cfg = MultiviewConfig::load_from_toml(&with_v4).unwrap();
    cfg.validate()
        .expect("an explicit IPv4 control.listen still validates");
    assert_eq!(
        cfg.control.as_ref().map(|c| c.listen.as_str()),
        Some("127.0.0.1:8080"),
    );
}

#[test]
fn control_listener_unparseable_addr_is_rejected() {
    let bad = format!("{BASE}\n[control]\nlisten = \"not-a-socket-addr\"\n");
    let cfg = MultiviewConfig::load_from_toml(&bad).unwrap();
    let err = cfg
        .validate()
        .expect_err("an unparseable control.listen must fail validation");
    assert!(
        err.to_string().contains("control.listen"),
        "error should name control.listen, got: {err}"
    );
}

#[test]
fn malformed_fps_string_is_rejected() {
    let bad = BASE.replace(r#"fps = "30000/1001""#, r#"fps = "not-a-ratio""#);
    assert!(
        MultiviewConfig::load_from_toml(&bad).is_err(),
        "malformed fps string must fail to parse"
    );
}

#[test]
fn zero_denominator_fps_is_rejected() {
    let bad = BASE.replace(r#"fps = "30000/1001""#, r#"fps = "30/0""#);
    // Either parse rejects it, or validate does; both are acceptable, but the
    // document must NOT be considered valid.
    if let Ok(cfg) = MultiviewConfig::load_from_toml(&bad) {
        assert!(
            cfg.validate().is_err(),
            "zero-denominator fps must not validate"
        );
    }
}

#[test]
fn cell_with_neither_area_nor_rect_is_rejected() {
    // Strip the `area = "d"` line from cell_d so it has no placement at all.
    let bad = BASE.replace("area = \"d\"\n", "");
    let cfg = MultiviewConfig::load_from_toml(&bad).unwrap();
    assert!(
        cfg.validate().is_err(),
        "a cell with neither area nor rect must fail"
    );
}

#[test]
fn absolute_rect_out_of_unit_range_is_rejected() {
    let doc = r##"
schema_version = 1
[canvas]
width = 1920
height = 1080
fps = "30000/1001"
pixel_format = "nv12"
background = "#000000"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "absolute"
[[sources]]
id = "in_main"
kind = "test"
[[cells]]
id = "cell_main"
z = 0
fit = "cover"
rect = { x = 0.5, y = 0.0, w = 0.9, h = 1.0 }
[cells.source]
input_id = "in_main"
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
"##;
    let cfg = MultiviewConfig::load_from_toml(doc).unwrap();
    // x + w = 1.4 > 1.0 -> rejected by core Layout::validate after solving.
    assert!(
        cfg.validate().is_err(),
        "rect exceeding unit range must fail"
    );
}
