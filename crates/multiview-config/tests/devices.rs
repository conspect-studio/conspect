//! Schema + validation + export tests for the managed-devices config-as-code
//! surface (ADR-M008): `[[devices]]` and `[[sync_groups]]`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{DeviceDriver, DisplayAssign, MultiviewConfig, SyncGroupMode};
use multiview_core::alarm::PerceivedSeverity;
use proptest::prelude::*;

/// A minimal, valid one-cell document used as the base every device/sync-group
/// fragment is appended to.
const BASE: &str = r##"
schema_version = 1

[canvas]
width = 1920
height = 1080
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
kind = "rtsp_server"
id = "out-main"
mount = "/multiview"
codec = "h264"
"##;

/// A 2x1 video wall declaring the `head-l`/`head-r` heads the brief's display
/// assignments reference.
const WALL: &str = r#"
[[walls]]
name = "lobby"
cols = 2
rows = 1
heads = [
  { id = "head-l", width = 1920, height = 1080, fps = "25/1", layout = "main" },
  { id = "head-r", width = 1920, height = 1080, fps = "25/1", layout = "main" },
]
"#;

/// The `[[devices]]`/`[[sync_groups]]` TOML sketch from
/// `docs/research/managed-devices.md` §7.3, **verbatim** (comments included).
const BRIEF_SKETCH: &str = r#"
[[devices]]
id = "dev-foyer-decoder"
display_name = "Foyer decoder"
driver = "zowietek"
address = "http://[fd00:db8::42]"        # IPv6-first; device itself may be IPv4-legacy
desired_mode = "decoder"
alarm_on_offline = "major"
[devices.auth]
secret_ref = "op://Site/foyer-decoder/credentials"   # write-only secret store; export = ref|redact
[devices.reconnect]
initial_ms = 500
max_ms = 30000

[[devices]]
id = "dev-node-left"
driver = "displaynode"                   # enrolled keypair identity, no password
[devices.display]
assign = { wall_head = "head-l" }        # or { program = true } / { output = "out-…" }

[[sync_groups]]
id = "lobby-wall"
mode = "auto"                            # auto = weakest-member tier
target_skew_ms = 50
members = [
  { device = "dev-node-left",     offset_ms = 0 },
  { device = "dev-node-right",    offset_ms = 0 },
  { device = "dev-foyer-decoder", offset_ms = 120 },
]
"#;

/// The third device the brief sketch's sync group references but does not
/// declare; appending it (plus [`WALL`]) completes the sketch into a document
/// that must validate green.
const SKETCH_COMPLETION: &str = r#"
[[devices]]
id = "dev-node-right"
driver = "displaynode"
[devices.display]
assign = { wall_head = "head-r" }
"#;

/// Compose [`BASE`] plus the given fragments into one document.
fn doc(fragments: &[&str]) -> String {
    let mut text = String::from(BASE);
    for fragment in fragments {
        text.push('\n');
        text.push_str(fragment);
    }
    text
}

/// Parse a composed document, panicking with the parse error on failure.
fn parse(fragments: &[&str]) -> MultiviewConfig {
    let text = doc(fragments);
    MultiviewConfig::load_from_toml(&text).expect("document parses")
}

// ---------------------------------------------------------------------------
// The brief's §7.3 sketch, verbatim-as-fixture.
// ---------------------------------------------------------------------------

#[test]
fn brief_sketch_parses_verbatim() {
    let cfg = parse(&[BRIEF_SKETCH]);

    assert_eq!(cfg.devices.len(), 2, "two devices declared");
    assert_eq!(cfg.sync_groups.len(), 1, "one sync group declared");

    let foyer = &cfg.devices[0];
    assert_eq!(foyer.id, "dev-foyer-decoder");
    assert_eq!(foyer.display_name.as_deref(), Some("Foyer decoder"));
    assert_eq!(foyer.driver, DeviceDriver::Zowietek);
    assert_eq!(foyer.address.as_deref(), Some("http://[fd00:db8::42]"));
    assert_eq!(foyer.desired_mode.as_deref(), Some("decoder"));
    assert_eq!(foyer.alarm_on_offline, Some(PerceivedSeverity::Major));
    let auth = foyer.auth.as_ref().expect("foyer decoder carries auth");
    assert_eq!(auth.secret_ref, "op://Site/foyer-decoder/credentials");
    let reconnect = foyer.reconnect.expect("foyer decoder carries reconnect");
    assert_eq!(reconnect.initial_ms, 500);
    assert_eq!(reconnect.max_ms, 30000);
    assert!(foyer.display.is_none(), "the decoder has no display facet");

    let node = &cfg.devices[1];
    assert_eq!(node.id, "dev-node-left");
    assert_eq!(node.driver, DeviceDriver::Displaynode);
    assert!(
        node.address.is_none(),
        "displaynode binds by enrolled identity"
    );
    assert!(node.auth.is_none(), "displaynode authenticates by keypair");
    let display = node.display.as_ref().expect("display node carries display");
    assert_eq!(display.assign, DisplayAssign::WallHead("head-l".to_owned()));

    let group = &cfg.sync_groups[0];
    assert_eq!(group.id, "lobby-wall");
    assert_eq!(group.mode, SyncGroupMode::Auto);
    assert_eq!(group.target_skew_ms, 50);
    assert_eq!(group.members.len(), 3);
    assert_eq!(group.members[0].device, "dev-node-left");
    assert_eq!(group.members[0].offset_ms, 0);
    assert_eq!(group.members[1].device, "dev-node-right");
    assert_eq!(group.members[1].offset_ms, 0);
    assert_eq!(group.members[2].device, "dev-foyer-decoder");
    assert_eq!(group.members[2].offset_ms, 120);
}

#[test]
fn brief_sketch_round_trips_toml_and_json() {
    let cfg = parse(&[WALL, BRIEF_SKETCH, SKETCH_COMPLETION]);

    let toml_text = cfg.to_toml().expect("serializes to TOML");
    let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses from TOML");
    assert_eq!(cfg, from_toml, "TOML round-trip is lossless");

    let json_text = cfg.to_json().expect("serializes to JSON");
    let from_json = MultiviewConfig::load_from_json(&json_text).expect("re-parses from JSON");
    assert_eq!(cfg, from_json, "JSON round-trip is lossless");
}

#[test]
fn brief_sketch_without_a_wall_fails_validation_naming_the_wall_head() {
    // The sketch assigns `head-l`, but no wall declares it.
    let cfg = parse(&[BRIEF_SKETCH]);
    let err = cfg.validate().expect_err("unbacked wall head must fail");
    assert!(err.to_string().contains("head-l"), "names the head: {err}");
}

#[test]
fn brief_sketch_with_wall_fails_validation_naming_the_dangling_member() {
    // `dev-node-right` is a sync-group member but never declared as a device.
    let cfg = parse(&[WALL, BRIEF_SKETCH]);
    let err = cfg.validate().expect_err("dangling member must fail");
    assert!(
        err.to_string().contains("dev-node-right"),
        "names the member: {err}"
    );
}

#[test]
fn completed_brief_sketch_validates() {
    let cfg = parse(&[WALL, BRIEF_SKETCH, SKETCH_COMPLETION]);
    cfg.validate().expect("the completed sketch is valid");
}

// ---------------------------------------------------------------------------
// Serde: drivers, severity tokens, display assignments, defaults.
// ---------------------------------------------------------------------------

#[test]
fn all_three_driver_variants_round_trip() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-zowie"
driver = "zowietek"
address = "http://[2001:db8::10]"

[[devices]]
id = "dev-node"
driver = "displaynode"

[[devices]]
id = "dev-cast"
driver = "cast"
address = "[2001:db8::20]:8009"
"#]);

    let drivers: Vec<DeviceDriver> = cfg.devices.iter().map(|d| d.driver).collect();
    assert_eq!(
        drivers,
        vec![
            DeviceDriver::Zowietek,
            DeviceDriver::Displaynode,
            DeviceDriver::Cast
        ]
    );

    // `DeviceDriver` is `#[non_exhaustive]`: downstream matches (this test
    // crate is downstream) must carry a wildcard arm, so a new compiled-in
    // family is not a breaking change.
    for device in &cfg.devices {
        let token = match device.driver {
            DeviceDriver::Zowietek => "zowietek",
            DeviceDriver::Displaynode => "displaynode",
            DeviceDriver::Cast => "cast",
            _ => "unknown",
        };
        assert_ne!(token, "unknown");
    }

    let toml_text = cfg.to_toml().expect("serializes to TOML");
    let reparsed = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses");
    assert_eq!(cfg, reparsed);
    let json_text = cfg.to_json().expect("serializes to JSON");
    let reparsed = MultiviewConfig::load_from_json(&json_text).expect("re-parses");
    assert_eq!(cfg, reparsed);
}

#[test]
fn unknown_driver_is_rejected_at_parse_time() {
    let text = doc(&[r#"
[[devices]]
id = "dev-x"
driver = "frobnicator"
"#]);
    let err = MultiviewConfig::load_from_toml(&text).expect_err("unknown driver must not parse");
    assert!(
        err.to_string().contains("frobnicator"),
        "names the bad driver token: {err}"
    );
}

#[test]
fn display_assign_program_and_output_forms_round_trip() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
[devices.display]
assign = { program = true }

[[devices]]
id = "dev-b"
driver = "displaynode"
[devices.display]
assign = { output = "out-main" }
"#]);

    assert_eq!(
        cfg.devices[0].display.as_ref().expect("display").assign,
        DisplayAssign::Program(true)
    );
    assert_eq!(
        cfg.devices[1].display.as_ref().expect("display").assign,
        DisplayAssign::Output("out-main".to_owned())
    );
    cfg.validate()
        .expect("program + declared-output assigns are valid");

    let toml_text = cfg.to_toml().expect("serializes to TOML");
    let reparsed = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses");
    assert_eq!(cfg, reparsed);
    let json_text = cfg.to_json().expect("serializes to JSON");
    let reparsed = MultiviewConfig::load_from_json(&json_text).expect("re-parses");
    assert_eq!(cfg, reparsed);
}

#[test]
fn severity_accepts_the_core_pascal_case_token() {
    // `PerceivedSeverity`'s own serde form is PascalCase ("Major", as probe
    // `severity` is authored); the device field tolerates it on input.
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
alarm_on_offline = "Major"
"#]);
    assert_eq!(
        cfg.devices[0].alarm_on_offline,
        Some(PerceivedSeverity::Major)
    );
}

#[test]
fn severity_serializes_as_the_lowercase_device_token() {
    // The device vocabulary is the brief's lowercase token ("major").
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
alarm_on_offline = "Major"
"#]);
    let toml_text = cfg.to_toml().expect("serializes to TOML");
    assert!(
        toml_text.contains("alarm_on_offline = \"major\""),
        "emits the lowercase token: {toml_text}"
    );
}

#[test]
fn unknown_severity_token_is_rejected_at_parse_time() {
    let text = doc(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
alarm_on_offline = "catastrophic"
"#]);
    let err = MultiviewConfig::load_from_toml(&text).expect_err("unknown severity must not parse");
    assert!(
        err.to_string().contains("catastrophic"),
        "names the bad token: {err}"
    );
}

#[test]
fn sync_group_mode_defaults_to_auto_and_member_offset_to_zero() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 50
members = [{ device = "dev-a" }]
"#]);
    assert_eq!(cfg.sync_groups[0].mode, SyncGroupMode::Auto);
    assert_eq!(cfg.sync_groups[0].members[0].offset_ms, 0);
    cfg.validate().expect("defaults are valid");
}

#[test]
fn existing_documents_without_devices_parse_with_empty_collections() {
    let cfg = MultiviewConfig::load_from_toml(BASE).expect("base document parses");
    assert!(cfg.devices.is_empty());
    assert!(cfg.sync_groups.is_empty());
    cfg.validate().expect("base document validates unchanged");
}

// ---------------------------------------------------------------------------
// Validation rejections: devices.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_device_ids_are_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[devices]]
id = "dev-a"
driver = "displaynode"
"#]);
    let err = cfg.validate().expect_err("duplicate device id must fail");
    assert!(err.to_string().contains("dev-a"), "names the id: {err}");
}

#[test]
fn empty_device_id_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = ""
driver = "displaynode"
"#]);
    let err = cfg.validate().expect_err("empty device id must fail");
    assert!(err.to_string().contains("id"), "names the field: {err}");
}

#[test]
fn zowietek_without_address_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
"#]);
    let err = cfg.validate().expect_err("zowietek needs an address");
    let msg = err.to_string();
    assert!(msg.contains("dev-a"), "names the device: {msg}");
    assert!(msg.contains("address"), "names the missing field: {msg}");
}

#[test]
fn cast_without_address_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "cast"
"#]);
    let err = cfg.validate().expect_err("cast needs an address");
    assert!(
        err.to_string().contains("address"),
        "names the field: {err}"
    );
}

#[test]
fn displaynode_without_address_validates() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
"#]);
    cfg.validate()
        .expect("displaynode binds by enrolled identity; no address required");
}

#[test]
fn empty_address_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = ""
"#]);
    let err = cfg.validate().expect_err("empty address must fail");
    assert!(
        err.to_string().contains("address"),
        "names the field: {err}"
    );
}

#[test]
fn empty_secret_ref_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
[devices.auth]
secret_ref = ""
"#]);
    let err = cfg.validate().expect_err("empty secret_ref must fail");
    assert!(
        err.to_string().contains("secret_ref"),
        "names the field: {err}"
    );
}

#[test]
fn empty_desired_mode_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
desired_mode = ""
"#]);
    let err = cfg.validate().expect_err("empty desired_mode must fail");
    assert!(
        err.to_string().contains("desired_mode"),
        "names the field: {err}"
    );
}

#[test]
fn inactive_offline_severity_is_rejected() {
    // `alarm_on_offline = "cleared"` is an inactive severity: omit the field
    // to disable the offline alarm instead.
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
alarm_on_offline = "cleared"
"#]);
    let err = cfg.validate().expect_err("inactive severity must fail");
    assert!(
        err.to_string().contains("alarm_on_offline"),
        "names the field: {err}"
    );
}

#[test]
fn reconnect_zero_initial_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
[devices.reconnect]
initial_ms = 0
max_ms = 30000
"#]);
    let err = cfg.validate().expect_err("zero initial_ms must fail");
    assert!(
        err.to_string().contains("initial_ms"),
        "names the field: {err}"
    );
}

#[test]
fn reconnect_max_below_initial_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "zowietek"
address = "http://[2001:db8::10]"
[devices.reconnect]
initial_ms = 5000
max_ms = 500
"#]);
    let err = cfg.validate().expect_err("max_ms < initial_ms must fail");
    assert!(err.to_string().contains("max_ms"), "names the field: {err}");
}

#[test]
fn display_assign_program_false_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
[devices.display]
assign = { program = false }
"#]);
    let err = cfg.validate().expect_err("program = false must fail");
    assert!(err.to_string().contains("program"), "names the form: {err}");
}

#[test]
fn display_assign_unknown_output_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
[devices.display]
assign = { output = "out-nope" }
"#]);
    let err = cfg.validate().expect_err("unknown output ref must fail");
    assert!(err.to_string().contains("out-nope"), "names the ref: {err}");
}

#[test]
fn display_assign_unknown_wall_head_is_rejected() {
    let cfg = parse(&[
        WALL,
        r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
[devices.display]
assign = { wall_head = "head-nope" }
"#,
    ]);
    let err = cfg.validate().expect_err("unknown wall head must fail");
    assert!(
        err.to_string().contains("head-nope"),
        "names the ref: {err}"
    );
}

// ---------------------------------------------------------------------------
// Validation rejections: sync groups.
// ---------------------------------------------------------------------------

#[test]
fn duplicate_sync_group_ids_are_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
[[devices]]
id = "dev-b"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 50
members = [{ device = "dev-a" }]

[[sync_groups]]
id = "g"
target_skew_ms = 50
members = [{ device = "dev-b" }]
"#]);
    let err = cfg.validate().expect_err("duplicate group id must fail");
    assert!(err.to_string().contains("\"g\""), "names the id: {err}");
}

#[test]
fn empty_sync_group_id_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = ""
target_skew_ms = 50
members = [{ device = "dev-a" }]
"#]);
    let err = cfg.validate().expect_err("empty group id must fail");
    assert!(err.to_string().contains("id"), "names the field: {err}");
}

#[test]
fn sync_group_with_no_members_is_rejected() {
    let cfg = parse(&[r#"
[[sync_groups]]
id = "g"
target_skew_ms = 50
members = []
"#]);
    let err = cfg.validate().expect_err("memberless group must fail");
    assert!(err.to_string().contains("member"), "names the rule: {err}");
}

#[test]
fn sync_group_member_referencing_unknown_device_is_rejected() {
    let cfg = parse(&[r#"
[[sync_groups]]
id = "g"
target_skew_ms = 50
members = [{ device = "dev-ghost" }]
"#]);
    let err = cfg.validate().expect_err("dangling member must fail");
    assert!(
        err.to_string().contains("dev-ghost"),
        "names the member: {err}"
    );
}

#[test]
fn duplicate_member_within_a_group_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 50
members = [{ device = "dev-a" }, { device = "dev-a", offset_ms = 10 }]
"#]);
    let err = cfg.validate().expect_err("duplicate member must fail");
    assert!(err.to_string().contains("dev-a"), "names the member: {err}");
}

#[test]
fn device_in_two_sync_groups_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"
[[devices]]
id = "dev-b"
driver = "displaynode"

[[sync_groups]]
id = "g1"
target_skew_ms = 50
members = [{ device = "dev-a" }, { device = "dev-b" }]

[[sync_groups]]
id = "g2"
target_skew_ms = 50
members = [{ device = "dev-a" }]
"#]);
    let err = cfg
        .validate()
        .expect_err("cross-group membership must fail");
    let msg = err.to_string();
    assert!(msg.contains("dev-a"), "names the device: {msg}");
    assert!(
        msg.contains("g1") && msg.contains("g2"),
        "names both groups: {msg}"
    );
}

#[test]
fn zero_target_skew_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 0
members = [{ device = "dev-a" }]
"#]);
    let err = cfg.validate().expect_err("zero target skew must fail");
    assert!(
        err.to_string().contains("target_skew_ms"),
        "names the field: {err}"
    );
}

#[test]
fn target_skew_above_ten_seconds_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 10001
members = [{ device = "dev-a" }]
"#]);
    let err = cfg.validate().expect_err("oversized target skew must fail");
    assert!(
        err.to_string().contains("target_skew_ms"),
        "names the field: {err}"
    );
}

#[test]
fn boundary_skew_and_offset_values_validate() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 10000
members = [{ device = "dev-a", offset_ms = 10000 }]
"#]);
    cfg.validate()
        .expect("10s skew/offset bounds are inclusive");
}

#[test]
fn member_offset_above_ten_seconds_is_rejected() {
    let cfg = parse(&[r#"
[[devices]]
id = "dev-a"
driver = "displaynode"

[[sync_groups]]
id = "g"
target_skew_ms = 50
members = [{ device = "dev-a", offset_ms = 10001 }]
"#]);
    let err = cfg.validate().expect_err("oversized offset must fail");
    assert!(
        err.to_string().contains("offset_ms"),
        "names the field: {err}"
    );
}

// ---------------------------------------------------------------------------
// Export semantics: desired state only; secrets stay refs.
// ---------------------------------------------------------------------------

#[test]
fn export_emits_desired_state_with_the_secret_ref_string_only() {
    let cfg = parse(&[WALL, BRIEF_SKETCH, SKETCH_COMPLETION]);
    cfg.validate().expect("fixture validates");

    let toml_text = cfg.to_toml().expect("serializes to TOML");

    // The credential exports as the ref string, verbatim.
    assert!(
        toml_text.contains("op://Site/foyer-decoder/credentials"),
        "the secret ref string survives export: {toml_text}"
    );

    // Desired state only: the export re-parses to exactly the authored
    // document — there is no field where runtime state (online state,
    // firmware, temperature, achieved skew) could appear.
    let reparsed = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses");
    assert_eq!(cfg, reparsed, "export is the authored desired state");

    // The same holds on the canonical JSON wire form.
    let json_text = cfg.to_json().expect("serializes to JSON");
    let value: serde_json::Value = serde_json::from_str(&json_text).expect("valid JSON");
    let devices = value
        .get("devices")
        .and_then(serde_json::Value::as_array)
        .expect("devices array exported");
    assert_eq!(devices.len(), 3);
    let auth = devices[0].get("auth").expect("auth block exported");
    assert_eq!(
        auth.get("secret_ref").and_then(serde_json::Value::as_str),
        Some("op://Site/foyer-decoder/credentials")
    );
    assert_eq!(
        auth.as_object().map(serde_json::Map::len),
        Some(1),
        "auth carries the ref and nothing else (no resolved secret)"
    );
}

// ---------------------------------------------------------------------------
// Property: generated device/sync-group documents round-trip and validate.
// ---------------------------------------------------------------------------

/// Build a document fragment with `n` devices (cycling the three drivers) and
/// one sync group containing every device once.
fn build_device_fragment(n: u8, target_skew_ms: u16, offsets: &[u16]) -> String {
    use std::fmt::Write as _;

    let mut s = String::new();
    for i in 0..n {
        writeln!(s, "[[devices]]").unwrap();
        writeln!(s, "id = \"dev-{i}\"").unwrap();
        match i % 3 {
            0 => {
                writeln!(s, "driver = \"zowietek\"").unwrap();
                writeln!(s, "address = \"http://[2001:db8::{i}]\"").unwrap();
            }
            1 => writeln!(s, "driver = \"displaynode\"").unwrap(),
            _ => {
                writeln!(s, "driver = \"cast\"").unwrap();
                writeln!(s, "address = \"[2001:db8::{i}]:8009\"").unwrap();
            }
        }
    }
    writeln!(s, "[[sync_groups]]").unwrap();
    writeln!(s, "id = \"g\"").unwrap();
    writeln!(s, "target_skew_ms = {target_skew_ms}").unwrap();
    let members = (0..n)
        // Cast devices (i % 3 == 2) are never sync participants (ADR-M011).
        .filter(|i| i % 3 != 2)
        .map(|i| {
            let offset = offsets.get(usize::from(i)).copied().unwrap_or(0);
            format!("{{ device = \"dev-{i}\", offset_ms = {offset} }}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(s, "members = [{members}]").unwrap();
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn generated_device_documents_round_trip_and_validate(
        n in 1u8..6,
        target_skew_ms in 1u16..=10_000,
        offsets in proptest::collection::vec(0u16..=10_000, 0..6),
    ) {
        let fragment = build_device_fragment(n, target_skew_ms, &offsets);
        let text = doc(&[fragment.as_str()]);
        let cfg = MultiviewConfig::load_from_toml(&text).expect("generated document parses");
        cfg.validate().expect("generated document validates");

        let toml_text = cfg.to_toml().expect("serializes to TOML");
        let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("re-parses TOML");
        prop_assert_eq!(&cfg, &from_toml);

        let json_text = cfg.to_json().expect("serializes to JSON");
        let from_json = MultiviewConfig::load_from_json(&json_text).expect("re-parses JSON");
        prop_assert_eq!(&cfg, &from_json);
    }
}

// ---------------------------------------------------------------------------
// Adversarial-review regressions (review of ship/dev-a1-device-config)
// ---------------------------------------------------------------------------

#[test]
fn unknown_device_key_is_rejected_at_parse() {
    // A typo'd or misplaced key must fail loudly: an inline `password`
    // (instead of `auth.secret_ref`) or a typo like `alarm_on_offine` must
    // never be silently dropped.
    let text =
        doc(&["[[devices]]\nid = \"d\"\ndriver = \"displaynode\"\npassword = \"hunter2\"\n"]);
    let err =
        MultiviewConfig::load_from_toml(&text).expect_err("unknown device key must not parse");
    assert!(err.to_string().contains("password"), "names the key: {err}");
}

#[test]
fn unknown_sync_group_key_is_rejected_at_parse() {
    let text = doc(&[
        "[[devices]]\nid = \"d\"\ndriver = \"displaynode\"\n\n[[sync_groups]]\nid = \"g\"\n\
         target_skew_ms = 50\nmembers = [{ device = \"d\" }]\nmode_extra = true\n",
    ]);
    let err =
        MultiviewConfig::load_from_toml(&text).expect_err("unknown sync-group key must not parse");
    assert!(
        err.to_string().contains("mode_extra"),
        "names the key: {err}"
    );
}

#[test]
fn cast_devices_are_rejected_as_sync_group_members() {
    // Cast is Tier D (seconds of receiver buffering, no sync surface) and is
    // never a sync participant (ADR-M011).
    let text = doc(&[
        "[[devices]]\nid = \"tv\"\ndriver = \"cast\"\naddress = \"[2001:db8::9]:8009\"\n\n\
         [[sync_groups]]\nid = \"g\"\ntarget_skew_ms = 50\nmembers = [{ device = \"tv\" }]\n",
    ]);
    let cfg = MultiviewConfig::load_from_toml(&text).expect("document parses");
    let err = cfg
        .validate()
        .expect_err("cast member must fail (Tier D, ADR-M011)");
    assert!(
        err.to_string().contains("never sync participants"),
        "states the rule: {err}"
    );
}

#[test]
fn indeterminate_offline_alarm_severity_is_rejected() {
    // `indeterminate` parses (it is a real X.733 severity token) but an
    // authored offline alarm must be definite.
    let text = doc(&[
        "[[devices]]\nid = \"d\"\ndriver = \"displaynode\"\nalarm_on_offline = \"indeterminate\"\n",
    ]);
    let cfg = MultiviewConfig::load_from_toml(&text).expect("document parses");
    let err = cfg
        .validate()
        .expect_err("indeterminate severity must fail");
    assert!(
        err.to_string().contains("definite active severity"),
        "states the rule: {err}"
    );
}

#[test]
fn driver_as_str_matches_serde_wire_form_and_display() {
    // The event `driver` strings (DEV-A3 / ADR-RT007) are constructed ONLY from
    // this wire form, never hand-typed — so `as_str()`, the serde rename, and
    // `Display` must agree for every variant.
    for (driver, wire) in [
        (DeviceDriver::Zowietek, "zowietek"),
        (DeviceDriver::Displaynode, "displaynode"),
        (DeviceDriver::Cast, "cast"),
    ] {
        assert_eq!(driver.as_str(), wire, "as_str() is the wire token");
        assert_eq!(driver.to_string(), wire, "Display matches as_str()");
        let serialised = serde_json::to_value(driver).unwrap();
        assert_eq!(
            serialised,
            serde_json::Value::String(wire.to_owned()),
            "serde wire form matches as_str()"
        );
    }
}
