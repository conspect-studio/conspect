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

/// Review L1-2: a cosmetic canvas edit keeps the accepted pinned signal.
#[test]
fn a_canvas_background_edit_keeps_the_accepted_resolution() {
    let prev = parse(&boot("/srv/ui-old"));
    let next = parse(&boot("/srv/ui-old").replace("#101014", "#202020"));
    let accepted = parse(
        &accepted()
            .replacen("width = 64", "width = 128", 1)
            .replacen("height = 64", "height = 128", 1),
    );
    let merged = merge_file_edit(&prev, &next, &accepted);
    assert_eq!((merged.canvas.width, merged.canvas.height), (128, 128));
    assert_eq!(merged.canvas.background, "#202020");
}

/// Review L1-3a: a cells-only file edit keeps the accepted binding of the
/// cell it did not touch.
#[test]
fn a_cells_only_edit_merges_cells_by_id() {
    let doc = |left: &str, right: &str| {
        let grid = boot("/srv/ui-old")
            .replace("columns = [\"1fr\"]", "columns = [\"1fr\", \"1fr\"]")
            .replace("areas = [\"a\"]", "areas = [\"a b\"]")
            .replace(
                "[[sources]]\nid = \"in_hls\"\nkind = \"bars\"\n",
                "[[sources]]\nid = \"in_hls\"\nkind = \"bars\"\n[[sources]]\nid = \"in_b\"\nkind = \"bars\"\n",
            )
            .replace(
                "[cells.source]\ninput_id = \"in_hls\"\n",
                &format!(
                    "[cells.source]\ninput_id = \"{left}\"\n[[cells]]\nid = \"d\"\narea = \"b\"\n[cells.source]\ninput_id = \"{right}\"\n"
                ),
            );
        parse(&grid)
    };
    let prev = doc("in_hls", "in_hls");
    let next = doc("in_hls", "in_b"); // the file rebinds `d`
    let accepted = doc("in_b", "in_hls"); // the API rebound `c`
    let merged = merge_file_edit(&prev, &next, &accepted);
    merged.validate().unwrap();
    assert_eq!(
        merged.cells[0], accepted.cells[0],
        "the API binding of c stays"
    );
    assert_eq!(
        merged.cells[1], next.cells[1],
        "the file's binding of d lands"
    );
}

/// Review L1-3b: an edit of one M/E field keeps the accepted other fields.
#[test]
fn a_switcher_field_edit_keeps_the_accepted_other_fields() {
    let me = |program: &str, flip: bool| {
        format!(
            "[switcher]\n[[switcher.mix_effects]]\nid = \"me1\"\nprogram = \"main\"\nprogram_source = \"{program}\"\npreview_source = \"in_c\"\nflip_flop = {flip}\n"
        )
    };
    let base = accepted();
    let prev = parse(&format!("{base}{}", me("in_b", true)));
    let next = parse(&format!("{base}{}", me("in_hls", true)));
    let accepted = parse(&format!("{base}{}", me("in_b", false)));
    let merged = merge_file_edit(&prev, &next, &accepted);
    let m = &merged.switcher.as_ref().unwrap().mix_effects[0];
    assert_eq!(m.program_source, "in_hls");
    assert!(!m.flip_flop, "the API-set flip_flop stays");
}

/// Review L1-1: a file edit of one IPTV key keeps the API-published entries,
/// and a removed entry is reported.
#[test]
fn an_iptv_key_edit_keeps_the_published_entries() {
    let iptv = |enabled: bool, entry: bool| {
        let mut text = format!("[iptv]\nenabled = {enabled}\nguide_refresh_minutes = 30\n");
        if entry {
            text.push_str("[[iptv.entries]]\nkind = \"output\"\nref = \"restream\"\n");
        }
        text
    };
    let file = boot("/srv/ui-old");
    let prev = parse(&format!("{file}{}", iptv(false, false)));
    let next = parse(&format!("{file}{}", iptv(true, false)));
    let accepted = parse(&format!("{}{}", accepted(), iptv(false, true)));
    let merged = merge_file_edit(&prev, &next, &accepted);
    let out = merged.iptv.as_ref().unwrap();
    assert!(out.enabled);
    assert_eq!(out.entries.len(), 1, "the published entry stays");
    let mut gone = merged.clone();
    gone.iptv.as_mut().unwrap().entries.clear();
    assert_eq!(
        removed_accepted_items(&accepted, &gone),
        ["iptv entry restream"]
    );
}

/// Review L1 round 2 (1): a file that resets a field to its serde default
/// (skipped on serialization) applies the reset, even while another field of
/// the same unit differs in the accepted state.
#[test]
fn a_reset_to_the_default_value_applies() {
    let me = |program: &str, flip: bool| {
        format!(
            "[switcher]\n[[switcher.mix_effects]]\nid = \"me1\"\nprogram = \"main\"\nprogram_source = \"{program}\"\npreview_source = \"in_c\"\nflip_flop = {flip}\n"
        )
    };
    let base = accepted();
    // prev: flip_flop = false (non-default); file resets it to the default true.
    let prev = parse(&format!("{base}{}", me("in_b", false)));
    let next = parse(&format!("{base}{}", me("in_b", true)));
    // The API changed program_source meanwhile.
    let accepted = parse(&format!("{base}{}", me("in_hls", false)));
    let merged = merge_file_edit(&prev, &next, &accepted);
    let m = &merged.switcher.as_ref().unwrap().mix_effects[0];
    assert!(m.flip_flop, "the file's reset to the default lands");
    assert_eq!(m.program_source, "in_hls", "the API-set field stays");

    // [iptv] enabled: true -> false (the default) while the API set a URL.
    let iptv = |enabled: bool, url: bool| {
        let mut t = format!("[iptv]\nenabled = {enabled}\n");
        if url {
            t.push_str("public_base_url = \"https://tv.example\"\n");
        }
        t
    };
    let file = boot("/srv/ui-old");
    let prev = parse(&format!("{file}{}", iptv(true, false)));
    let next = parse(&format!("{file}{}", iptv(false, false)));
    let accepted = parse(&format!("{}{}", self::accepted(), iptv(true, true)));
    let merged = merge_file_edit(&prev, &next, &accepted);
    let out = merged.iptv.unwrap();
    assert!(!out.enabled, "the file's reset to the default lands");
    assert_eq!(out.public_base_url.as_deref(), Some("https://tv.example"));
}

/// Review L1 round 2 (2a): a file edit of one IPTV entry field keeps the
/// API-set other fields of that entry.
#[test]
fn an_iptv_entry_field_edit_keeps_the_api_set_fields() {
    let entry = |name: &str, logo: bool| {
        let mut t = format!(
            "[iptv]\nenabled = true\n[[iptv.entries]]\nkind = \"output\"\nref = \"restream\"\nname = \"{name}\"\n"
        );
        if logo {
            t.push_str("logo = \"https://tv.example/logo.png\"\n");
        }
        t
    };
    let file = boot("/srv/ui-old");
    let prev = parse(&format!("{file}{}", entry("Old", false)));
    let next = parse(&format!("{file}{}", entry("New", false)));
    let accepted = parse(&format!("{}{}", accepted(), entry("Old", true)));
    let merged = merge_file_edit(&prev, &next, &accepted);
    let e = &merged.iptv.as_ref().unwrap().entries[0];
    assert_eq!(e.name.as_deref(), Some("New"));
    assert_eq!(e.logo.as_deref(), Some("https://tv.example/logo.png"));
}

/// Review L1 round 2 (2b): a canvas colour profile edit keeps API-set colour
/// axes.
#[test]
fn a_canvas_colour_profile_edit_keeps_the_other_colour_axes() {
    let prev = parse(&boot("/srv/ui-old"));
    let next = parse(&boot("/srv/ui-old").replace("sdr-bt709-limited", "sdr-bt709-full"));
    if next.canvas.color == prev.canvas.color {
        return;
    }
    let mut accepted = parse(&accepted());
    accepted.canvas.color.transfer = Some("bt709".to_owned());
    let merged = merge_file_edit(&prev, &next, &accepted);
    assert_eq!(merged.canvas.color.profile, next.canvas.color.profile);
    assert_eq!(merged.canvas.color.transfer.as_deref(), Some("bt709"));
}
