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

/// ADR-0037 / REC-8: an existing document gains recording without changing,
/// and a declared recording is checked against the rules in the brief.
#[test]
fn recording_is_additive_and_its_rules_reject_bad_input() {
    let cfg = MultiviewConfig::load_from_toml(BASE).unwrap();
    assert!(cfg.sources.iter().all(|source| source.iso.is_none()));
    assert!(cfg.outputs.iter().all(|output| output.program().is_none()));
    let rewritten = toml::to_string(&cfg).unwrap();
    assert!(
        !rewritten.contains("[outputs.program]"),
        "an unrecorded document must not grow a recording section: {rewritten}"
    );

    let good = format!(
        "{BASE}\n\
         [[sources]]\n\
         id = \"cam1\"\n\
         kind = \"srt\"\n\
         url = \"srt://[::1]:9000\"\n\
         [sources.iso]\n\
         armed = true\n\
         location = \"/var/lib/conspect/rec/cam1\"\n\
         [sources.iso.segment]\n\
         duration_seconds = 6\n\
         [sources.iso.retention]\n\
         keep_duration_seconds = 3600\n\
         [sources.iso.disk]\n\
         min_free_gib = 10\n\
         stop_at_pct = 80\n\
         \n\
         [outputs.program]\n\
         armed = true\n\
         [outputs.program.segment]\n\
         duration_seconds = 6\n\
         [outputs.program.retention]\n\
         keep_size_gib = 50\n\
         [outputs.program.disk]\n\
         min_free_gib = 10\n\
         stop_at_pct = 80\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&good).unwrap();
    cfg.validate().expect("a well-formed recording validates");
    let cam = cfg.sources.iter().find(|s| s.id == "cam1").unwrap();
    assert!(cam.iso.as_ref().unwrap().armed);
    let program = cfg.outputs[0].program();
    assert!(program.unwrap().armed);

    let json = serde_json::to_string(&cfg).unwrap();
    let back = MultiviewConfig::load_from_json(&json).unwrap();
    assert_eq!(back.outputs[0].program(), cfg.outputs[0].program());
    assert_eq!(
        back.sources.iter().find(|s| s.id == "cam1").unwrap().iso,
        cam.iso
    );

    let unarmed = good.replace("armed = true", "armed = false");
    let mut broken = unarmed.replace("keep_duration_seconds = 3600\n", "");
    broken = broken.replace("armed = false", "armed = true");
    let cfg = MultiviewConfig::load_from_toml(&broken).unwrap();
    let err = cfg.validate().expect_err("no retention bound");
    assert!(err.to_string().contains("retention"), "{err}");

    let bad_pct = good.replace("stop_at_pct = 80", "stop_at_pct = 100");
    let cfg = MultiviewConfig::load_from_toml(&bad_pct).unwrap();
    let err = cfg.validate().expect_err("stop_at_pct 100");
    assert!(err.to_string().contains("stop_at_pct"), "{err}");

    let zero = good.replace("duration_seconds = 6", "duration_seconds = 0");
    let cfg = MultiviewConfig::load_from_toml(&zero).unwrap();
    let err = cfg.validate().expect_err("zero duration");
    assert!(err.to_string().contains("duration_seconds"), "{err}");

    let byte_exact = format!("{good}\n[sources.iso.container]\nkind = \"byte_exact\"\n");
    let cfg = MultiviewConfig::load_from_toml(&byte_exact).unwrap();
    cfg.validate()
        .expect("byte_exact is legal on an srt source");

    let bars_exact = byte_exact.replace(
        "kind = \"srt\"\nurl = \"srt://[::1]:9000\"\n",
        "kind = \"bars\"\n",
    );
    let cfg = MultiviewConfig::load_from_toml(&bars_exact).unwrap();
    let err = cfg.validate().expect_err("byte_exact on bars");
    assert!(err.to_string().contains("byte_exact"), "{err}");
}

#[test]
fn source_capacity_is_validated_without_silent_truncation() {
    let mut cfg = MultiviewConfig::load_from_toml(BASE).unwrap();
    let exemplar = cfg.sources[0].clone();
    while cfg.sources.len() < multiview_config::MAX_ADMITTED_SOURCES {
        let mut source = exemplar.clone();
        source.id = format!("admitted-{}", cfg.sources.len());
        cfg.sources.push(source);
    }
    cfg.validate().expect("exact source capacity accepted");
    let mut excess = exemplar;
    excess.id = "over-capacity".to_owned();
    cfg.sources.push(excess);
    let error = cfg.validate().expect_err("source above capacity rejected");
    assert!(error.to_string().contains("source capacity"), "{error}");
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
fn control_device_dial_allow_valid_cidrs_are_accepted() {
    // The SSRF dial allowlist (ADR-M013): a list of CIDRs (IPv6-first) an
    // operator locks the outbound device dial to. Valid entries validate.
    let doc = format!(
        "{BASE}\n[control]\nlisten = \"[::]:8080\"\n\
         device_dial_allow = [\"192.168.0.0/16\", \"fd00:db8::/32\", \"10.5.0.7\"]\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    cfg.validate()
        .expect("a control.device_dial_allow of valid CIDRs must validate");
}

#[test]
fn control_device_dial_allow_malformed_cidr_is_rejected() {
    // A typo in the allowlist must fail at config-validation time (fail-closed),
    // not be silently ignored (which would leave the dial screen wider than the
    // operator intended). Proves the field is deserialized AND validated: an
    // ignored/unwired field would never see the bad CIDR.
    let doc = format!(
        "{BASE}\n[control]\nlisten = \"[::]:8080\"\n\
         device_dial_allow = [\"192.168.0.0/16\", \"not-a-cidr\"]\n"
    );
    let cfg = MultiviewConfig::load_from_toml(&doc).unwrap();
    let err = cfg
        .validate()
        .expect_err("a malformed control.device_dial_allow entry must fail validation");
    assert!(
        err.to_string().contains("device_dial_allow"),
        "error should name device_dial_allow, got: {err}"
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
