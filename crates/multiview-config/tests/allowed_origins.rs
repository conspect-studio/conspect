//! SEC-13 (ADR-RT011): `control.allowed_origins` entries are strictly-parsed
//! serialized origins (`scheme://host[:port]`, and nothing else). A malformed
//! entry must FAIL at config-load — a silently never-matching allow-list entry is
//! a fail-open usability trap (the operator believes an origin is permitted when
//! it can never match).
//!
//! These tests pin the six shapes the pre-fix validator wrongly accepted (it only
//! rejected an empty string or one lacking `"://"`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::MultiviewConfig;

/// A minimal valid document with a `[control]` section; `control_extra` is spliced
/// into it (e.g. an `allowed_origins = [...]` line). Mirrors the `cast_control`
/// fixture so the only variable under test is the control-plane knob.
fn base(control_extra: &str) -> String {
    format!(
        r##"
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
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"

[[outputs]]
id = "out-a"
kind = "hls"
path = "/srv/hls/out-a/a.m3u8"
codec = "mpeg2video"
segment_ms = 2000

[control]
listen = "[::]:8080"
{control_extra}
"##
    )
}

/// The six malformed origins the pre-fix validator wrongly accepted. Each is a
/// distinct way the naive `contains("://")` shape check fails open.
const BAD_ORIGINS: &[&str] = &[
    "://",               // empty scheme + empty host
    "https://",          // empty host
    "https://user@host", // userinfo
    "https://host/path", // path
    "https://host?x",    // query
    "garbage://host",    // non-http(s) scheme
];

/// Each of the six malformed entries must be REJECTED at config-load — not stored
/// as a never-matching allow-list line.
#[test]
fn rejects_each_malformed_allowed_origin() {
    for bad in BAD_ORIGINS {
        let toml = base(&format!("allowed_origins = [\"{bad}\"]"));
        let cfg = MultiviewConfig::load_from_toml(&toml).unwrap_or_else(|e| {
            panic!("{bad:?} is valid TOML — the origin check is semantic (validate): {e}")
        });
        assert!(
            cfg.validate().is_err(),
            "control.allowed_origins = [{bad:?}] must be rejected at config-load \
             (it is not a bare scheme://host[:port] origin)"
        );
    }
}

/// Well-formed origins — including a bracketed IPv6 literal with a port — still
/// validate. Guards the fixture: if `base()` were invalid for an unrelated reason
/// this positive control fails, so a rejection above can only be the origin check.
#[test]
fn accepts_well_formed_allowed_origins() {
    let toml = base(
        "allowed_origins = [\"https://ops.example\", \"http://mv.local:8080\", \
         \"http://[2001:db8::7]:8443\"]",
    );
    let cfg = MultiviewConfig::load_from_toml(&toml).unwrap();
    cfg.validate()
        .expect("well-formed scheme://host[:port] origins validate");
}
