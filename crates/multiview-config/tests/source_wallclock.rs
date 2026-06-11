//! Per-source `wallclock` config field (ADR-0038, SYNC-0): the operator's
//! Use/Discard verb on a `Source` parses, defaults to `Use`, is additive (existing
//! configs are unchanged), and survives TOML + JSON round-trips.
//!
//! Trust TIER is measured at runtime, never authored — config carries only the
//! `use`/`discard` verb.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::schema::{SourceWallClock, WallClockUse};
use multiview_config::MultiviewConfig;

/// A structurally-complete one-source grid document; `wallclock_line` is spliced
/// into the single source block (empty string = no `wallclock` field at all).
fn doc_with_wallclock(wallclock_line: &str) -> String {
    format!(
        "schema_version = 1\n\
         [canvas]\n\
         width = 1280\n\
         height = 720\n\
         fps = \"25/1\"\n\
         pixel_format = \"nv12\"\n\
         background = \"#101014\"\n\
         [canvas.color]\n\
         profile = \"sdr-bt709-limited\"\n\
         [layout]\n\
         kind = \"grid\"\n\
         columns = [\"1fr\"]\n\
         rows = [\"1fr\"]\n\
         gap = 0\n\
         areas = [\"c0\"]\n\
         [[sources]]\n\
         id = \"in_0\"\n\
         kind = \"hls\"\n\
         url = \"https://example.com/index.m3u8\"\n\
         {wallclock_line}\n\
         [[cells]]\n\
         id = \"cell_0\"\n\
         area = \"c0\"\n\
         fit = \"contain\"\n\
         [cells.source]\n\
         input_id = \"in_0\"\n\
         [[outputs]]\n\
         kind = \"rtsp_server\"\n\
         mount = \"/multiview\"\n\
         codec = \"h264\"\n"
    )
}

#[test]
fn wallclock_absent_is_none_and_existing_configs_are_unchanged() {
    // Additive: an existing config without the field parses and validates exactly
    // as before (default-off ⇒ zero behaviour change at the schema layer).
    let cfg = MultiviewConfig::load_from_toml(&doc_with_wallclock("")).expect("parse");
    assert_eq!(cfg.sources[0].wallclock, None);
    assert!(cfg.validate().is_ok());
}

#[test]
fn wallclock_use_parses_and_round_trips_through_toml_and_json() {
    let cfg = MultiviewConfig::load_from_toml(&doc_with_wallclock("wallclock = { use = \"use\" }"))
        .expect("parse");
    assert_eq!(
        cfg.sources[0].wallclock,
        Some(SourceWallClock {
            use_: WallClockUse::Use
        })
    );

    let json = cfg.to_json().expect("to_json");
    assert_eq!(
        MultiviewConfig::load_from_json(&json).expect("from_json"),
        cfg
    );

    let toml_text = cfg.to_toml().expect("to_toml");
    assert_eq!(
        MultiviewConfig::load_from_toml(&toml_text).expect("from_toml"),
        cfg
    );
}

#[test]
fn wallclock_discard_parses_to_its_variant() {
    let cfg =
        MultiviewConfig::load_from_toml(&doc_with_wallclock("wallclock = { use = \"discard\" }"))
            .expect("parse");
    assert_eq!(
        cfg.sources[0].wallclock,
        Some(SourceWallClock {
            use_: WallClockUse::Discard
        })
    );
}

#[test]
fn wallclock_use_default_is_use_when_field_present_without_verb() {
    // `wallclock = {}` -> the verb defaults to Use (serde default), matching the
    // ADR's "default Use" stance; the toggle is opt-out-of-Use.
    let cfg =
        MultiviewConfig::load_from_toml(&doc_with_wallclock("wallclock = {}")).expect("parse");
    assert_eq!(
        cfg.sources[0].wallclock,
        Some(SourceWallClock {
            use_: WallClockUse::Use
        })
    );
}

#[test]
fn wallclock_verb_serializes_as_a_snake_case_tag() {
    assert_eq!(
        serde_json::to_string(&WallClockUse::Discard).expect("ser"),
        "\"discard\""
    );
    assert_eq!(
        serde_json::to_string(&WallClockUse::Use).expect("ser"),
        "\"use\""
    );
}
