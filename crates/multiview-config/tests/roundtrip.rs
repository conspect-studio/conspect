//! Property tests: a generated document survives TOML and JSON round-trips
//! unchanged, and `fps` rational strings parse back to the same exact rational.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fmt::Write as _;

use multiview_config::MultiviewConfig;
use proptest::prelude::*;

/// Build a small but structurally complete grid document from generated knobs.
fn build_doc(width: u32, height: u32, fps_num: u32, fps_den: u32, gap: u32, n: u8) -> String {
    let cols = vec!["\"1fr\""; usize::from(n)].join(", ");
    let areas = (0..n)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(" ");

    let mut s = String::new();
    writeln!(s, "schema_version = 1").unwrap();
    writeln!(s, "[canvas]").unwrap();
    writeln!(s, "width = {width}").unwrap();
    writeln!(s, "height = {height}").unwrap();
    writeln!(s, "fps = \"{fps_num}/{fps_den}\"").unwrap();
    writeln!(s, "pixel_format = \"nv12\"").unwrap();
    writeln!(s, "background = \"#101014\"").unwrap();
    writeln!(s, "[canvas.color]").unwrap();
    writeln!(s, "profile = \"sdr-bt709-limited\"").unwrap();
    writeln!(s, "[layout]").unwrap();
    writeln!(s, "kind = \"grid\"").unwrap();
    writeln!(s, "columns = [{cols}]").unwrap();
    writeln!(s, "rows = [\"1fr\"]").unwrap();
    writeln!(s, "gap = {gap}").unwrap();
    writeln!(s, "areas = [\"{areas}\"]").unwrap();
    for i in 0..n {
        writeln!(s, "[[sources]]").unwrap();
        writeln!(s, "id = \"in_{i}\"").unwrap();
        writeln!(s, "kind = \"test\"").unwrap();
    }
    for i in 0..n {
        writeln!(s, "[[cells]]").unwrap();
        writeln!(s, "id = \"cell_{i}\"").unwrap();
        writeln!(s, "area = \"c{i}\"").unwrap();
        writeln!(s, "fit = \"contain\"").unwrap();
        writeln!(s, "[cells.source]").unwrap();
        writeln!(s, "input_id = \"in_{i}\"").unwrap();
    }
    writeln!(s, "[[outputs]]").unwrap();
    writeln!(s, "kind = \"rtsp_server\"").unwrap();
    writeln!(s, "mount = \"/multiview\"").unwrap();
    writeln!(s, "codec = \"h264\"").unwrap();
    s
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn toml_json_round_trip_is_lossless(
        width in 16u32..7680,
        height in 16u32..4320,
        fps_num in 1u32..120_000,
        fps_den in 1u32..1002,
        gap in 0u32..64,
        n in 1u8..6,
    ) {
        let doc = build_doc(width, height, fps_num, fps_den, gap, n);
        let cfg = MultiviewConfig::load_from_toml(&doc)
            .expect("generated document should parse");

        // The parsed fps must equal the exact source rational.
        let r = cfg.canvas.fps.rational();
        prop_assert_eq!(r.num, i64::from(fps_num));
        prop_assert_eq!(r.den, i64::from(fps_den));

        // JSON round-trip.
        let json = cfg.to_json().expect("to_json");
        let from_json = MultiviewConfig::load_from_json(&json).expect("from_json");
        prop_assert_eq!(&cfg, &from_json);

        // TOML round-trip.
        let toml_text = cfg.to_toml().expect("to_toml");
        let from_toml = MultiviewConfig::load_from_toml(&toml_text).expect("from_toml");
        prop_assert_eq!(&cfg, &from_toml);
    }

    #[test]
    fn generated_grid_documents_validate(
        fps_num in 1u32..120_000,
        fps_den in 1u32..1002,
        gap in 0u32..32,
        n in 1u8..6,
    ) {
        let doc = build_doc(1920, 1080, fps_num, fps_den, gap, n);
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
        prop_assert!(cfg.validate().is_ok());
    }
}
