//! `[system.youtube]` config surface (ADR-0015 §6): the optional explicit
//! `yt-dlp` binary path, so the service user's `PATH` need not carry it.
//!
//! Pure serde + load — no engine, no `youtube` feature required (the field is
//! plain data the resolver reads at the CLI boundary). Load-bearing properties:
//! the field is optional with a serde default, so an existing document with no
//! `[system.youtube]` section still loads unchanged, and a configured path
//! round-trips losslessly.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{MultiviewConfig, SystemConfig, YoutubeSystemConfig};

/// A minimal, valid 2x2 grid document used as the base for `[system.youtube]`
/// mutation (mirrors `tests/system_ndi.rs`).
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
fn an_absent_system_youtube_section_is_none() {
    let sys: SystemConfig = toml::from_str("").expect("an empty [system] table deserializes");
    assert!(sys.youtube.is_none());
}

#[test]
fn an_explicit_yt_dlp_path_round_trips() {
    let sys: SystemConfig = toml::from_str(
        r#"
[youtube]
yt_dlp_path = "/opt/yt-dlp/yt-dlp"
"#,
    )
    .expect("a [system.youtube] block deserializes");

    let youtube = sys.youtube.as_ref().expect("youtube sub-table present");
    assert_eq!(youtube.yt_dlp_path.as_deref(), Some("/opt/yt-dlp/yt-dlp"));

    let dumped = toml::to_string(&sys).expect("serializes");
    assert!(
        dumped.contains("/opt/yt-dlp/yt-dlp"),
        "the explicit path must export plainly, got:\n{dumped}"
    );
}

#[test]
fn a_document_with_no_system_youtube_section_still_loads() {
    // Byte-for-byte the existing-document guarantee: no `[system.youtube]` at
    // all must not fail to load or default to anything but `None` (resolve on
    // `PATH`, the resolver's own default).
    let cfg = MultiviewConfig::load_from_toml(BASE).expect("loads with no [system] section");
    assert!(cfg.system.is_none());
}

#[test]
fn a_configured_yt_dlp_path_survives_load_and_validate() {
    let doc = format!("{BASE}\n[system.youtube]\nyt_dlp_path = \"/usr/local/bin/yt-dlp\"\n");
    let cfg = MultiviewConfig::load_from_toml(&doc).expect("loads");
    cfg.validate()
        .expect("a configured yt_dlp_path must validate");
    let youtube = cfg
        .system
        .as_ref()
        .and_then(|s| s.youtube.as_ref())
        .expect("system.youtube present");
    assert_eq!(
        youtube.yt_dlp_path.as_deref(),
        Some("/usr/local/bin/yt-dlp")
    );
}

#[test]
fn an_empty_system_youtube_table_constructs_via_default() {
    // `YoutubeSystemConfig` is a plain constructable data struct (like
    // `NdiSystemConfig`), so callers in other crates can build one directly.
    let youtube = YoutubeSystemConfig::default();
    assert!(youtube.yt_dlp_path.is_none());
}
