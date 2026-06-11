//! Per-source caption selector (#24): the `captions` field on a source parses,
//! defaults to absent (no decode), and survives TOML + JSON round-trips.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::schema::CaptionSelector;
use multiview_config::MultiviewConfig;

/// A structurally-complete one-source grid document; `captions_line` is spliced
/// into the single source block (empty string = no `captions` field at all).
fn doc_with_captions(captions_line: &str) -> String {
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
         kind = \"test\"\n\
         {captions_line}\n\
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
fn captions_absent_is_none_and_document_still_validates() {
    // The selector is opt-in: an existing config without it is unchanged and the
    // engine decodes no caption track it will not show.
    let cfg = MultiviewConfig::load_from_toml(&doc_with_captions("")).expect("parse");
    assert_eq!(cfg.sources[0].captions, None);
    assert!(cfg.validate().is_ok());
}

#[test]
fn teletext_page_selector_round_trips_through_toml_and_json() {
    let cfg = MultiviewConfig::load_from_toml(&doc_with_captions(
        "captions = { mode = \"teletext_page\", page = 801 }",
    ))
    .expect("parse");
    assert_eq!(
        cfg.sources[0].captions,
        Some(CaptionSelector::TeletextPage { page: 801 })
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
fn every_selector_mode_parses_to_its_variant() {
    let cases = [
        ("captions = { mode = \"auto\" }", CaptionSelector::Auto),
        ("captions = { mode = \"off\" }", CaptionSelector::Off),
        (
            "captions = { mode = \"embedded_cc\", field = \"cc1\" }",
            CaptionSelector::EmbeddedCc {
                field: "cc1".to_owned(),
            },
        ),
        (
            "captions = { mode = \"track\", id = \"eng\" }",
            CaptionSelector::Track {
                id: "eng".to_owned(),
            },
        ),
        (
            "captions = { mode = \"sidecar\", path = \"/subs.vtt\" }",
            CaptionSelector::Sidecar {
                path: "/subs.vtt".to_owned(),
            },
        ),
    ];
    for (line, want) in cases {
        let cfg = MultiviewConfig::load_from_toml(&doc_with_captions(line)).expect("parse");
        assert_eq!(cfg.sources[0].captions.as_ref(), Some(&want));
    }
}

#[test]
fn a_mode_less_captions_table_is_rejected() {
    // Internally-tagged: a `captions` table without `mode` must not silently
    // deserialize (no `untagged` ambiguity).
    let err = MultiviewConfig::load_from_toml(&doc_with_captions("captions = { page = 801 }"));
    assert!(err.is_err(), "a mode-less captions table must be rejected");
}
