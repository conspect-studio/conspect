//! #367: a boot-file edit applies only what the FILE edit changed onto the
//! accepted state; every untouched section keeps the accepted value.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use multiview_config::{merge_file_edit, removed_accepted_items, MultiviewConfig};

const HEAD: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
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
"##;

/// The small boot file: one source, one cell, one output, no switcher/audio.
fn boot(ui_dir: &str) -> String {
    format!(
        r#"{HEAD}[control]
listen = "[::1]:0"
ui_dir = "{ui_dir}"
[[sources]]
id = "in_hls"
kind = "bars"
[[cells]]
id = "c"
area = "a"
[cells.source]
input_id = "in_hls"
[[outputs]]
kind = "hls"
id = "restream"
path = "/tmp/restream.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"#
    )
}

/// The accepted state the operator built: three sources, two outputs, audio.
fn accepted() -> String {
    format!(
        r##"{HEAD}[control]
listen = "[::1]:0"
ui_dir = "/srv/ui-old"
[[sources]]
id = "in_hls"
kind = "bars"
[[sources]]
id = "in_b"
kind = "solid"
color = "#101418"
[[sources]]
id = "in_c"
kind = "bars"
[[cells]]
id = "c"
area = "a"
[cells.source]
input_id = "in_b"
[[outputs]]
kind = "hls"
id = "restream"
path = "/tmp/restream.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[[outputs]]
kind = "hls"
id = "second"
path = "/tmp/second.m3u8"
codec = "mpeg2video"
segment_ms = 1000
[audio]
sample_rate_hz = 48000
[[audio.routes]]
input_id = "in_b"
target_track = "clean"
include_in_program_bus = true
[audio.routes.channels]
kind = "stereo"
"##
    )
}

fn parse(text: &str) -> MultiviewConfig {
    let config = MultiviewConfig::load_from_toml(text).expect("parses");
    config.validate().expect("validates");
    config
}

#[test]
fn a_one_key_edit_in_a_restart_section_keeps_every_other_accepted_section() {
    let prev = parse(&boot("/srv/ui-old"));
    let next = parse(&boot("/srv/ui-new"));
    let accepted = parse(&accepted());
    let merged = merge_file_edit(&prev, &next, &accepted);
    merged.validate().expect("merged validates");
    // The edited key lands.
    assert_eq!(
        merged.control.as_ref().unwrap().ui_dir.as_deref(),
        Some(std::path::Path::new("/srv/ui-new"))
    );
    // Nothing else moved.
    let mut expected = accepted.clone();
    expected.control.clone_from(&merged.control);
    assert_eq!(merged, expected);
    assert_eq!(
        removed_accepted_items(&accepted, &merged),
        Vec::<String>::new()
    );
}

#[test]
fn an_unchanged_file_changes_nothing() {
    let prev = parse(&boot("/srv/ui-old"));
    let accepted = parse(&accepted());
    assert_eq!(merge_file_edit(&prev, &prev, &accepted), accepted);
}

#[test]
fn a_file_item_edit_changes_only_that_item() {
    let prev = parse(&boot("/srv/ui-old"));
    let next = parse(
        &boot("/srv/ui-old").replace("kind = \"bars\"", "kind = \"solid\"\ncolor = \"#202020\""),
    );
    let accepted = parse(&accepted());
    let merged = merge_file_edit(&prev, &next, &accepted);
    merged.validate().unwrap();
    assert_eq!(merged.sources.len(), 3);
    assert_eq!(merged.sources[0], next.sources[0]);
    assert_eq!(merged.sources[1..], accepted.sources[1..]);
    assert_eq!(merged.outputs, accepted.outputs);
    assert_eq!(merged.audio, accepted.audio);
    assert_eq!(merged.cells, accepted.cells);
}

#[test]
fn a_file_add_is_appended_and_a_file_removal_removes_only_that_item() {
    let prev = parse(&boot("/srv/ui-old"));
    let added = format!(
        "{}[[sources]]\nid = \"in_new\"\nkind = \"bars\"\n",
        boot("/srv/ui-old")
    );
    let accepted = parse(&accepted());
    let merged = merge_file_edit(&prev, &parse(&added), &accepted);
    let ids: Vec<&str> = merged.sources.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["in_hls", "in_b", "in_c", "in_new"]);

    // The file replaces its only output `restream` with `third`: only
    // `restream` goes, the accepted `second` stays, `third` is added.
    let swapped = parse(
        &boot("/srv/ui-old")
            .replace("id = \"restream\"", "id = \"third\"")
            .replace("/tmp/restream.m3u8", "/tmp/third.m3u8"),
    );
    let merged = merge_file_edit(&prev, &swapped, &accepted);
    let outs: Vec<String> = merged
        .outputs
        .iter()
        .map(multiview_config::Output::id)
        .collect();
    assert_eq!(outs, ["second", "third"]);
    assert_eq!(
        removed_accepted_items(&accepted, &merged),
        ["output restream"]
    );
}

#[test]
fn a_whole_file_replacement_is_reported_item_by_item() {
    // What #367 did: the whole small file committed over the accepted state.
    let accepted = parse(&accepted());
    let file = parse(&boot("/srv/ui-new"));
    let removed = removed_accepted_items(&accepted, &file);
    assert_eq!(
        removed,
        [
            "source in_b",
            "source in_c",
            "output second",
            "audio route in_b -> clean"
        ]
    );
}

#[test]
fn a_file_audio_route_edit_keeps_the_accepted_routes() {
    let route = |input: &str, track: &str| {
        format!(
            "[[audio.routes]]\ninput_id = \"{input}\"\ntarget_track = \"{track}\"\ninclude_in_program_bus = true\n[audio.routes.channels]\nkind = \"stereo\"\n"
        )
    };
    let prev = parse(&boot("/srv/ui-old"));
    let next = parse(&format!(
        "{}[audio]\nsample_rate_hz = 48000\n{}",
        boot("/srv/ui-old"),
        route("in_hls", "file")
    ));
    let accepted = parse(&accepted());
    let merged = merge_file_edit(&prev, &next, &accepted);
    merged.validate().unwrap();
    let tracks: Vec<Option<String>> = merged
        .audio
        .unwrap()
        .routes
        .iter()
        .map(|r| r.target_track.clone())
        .collect();
    assert_eq!(tracks, [Some("clean".to_owned()), Some("file".to_owned())]);
}
