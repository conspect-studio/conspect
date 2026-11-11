//! The one shared definition of a source's media-affecting fields
//! (`Source::same_media`, review R3-02): presentation (EPG, playlist
//! metadata) and standing policy (automation, activation) never change media;
//! everything else, including `events.observe`, does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use multiview_config::Source;
use serde_json::json;

fn source(value: serde_json::Value) -> Source {
    serde_json::from_value(value).expect("valid source")
}

#[test]
fn presentation_and_policy_together_keep_the_same_media() {
    let base = source(json!({"id": "w", "kind": "hls", "url": "https://example.test/w.m3u8"}));
    let edited = source(
        json!({"id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
        "activation": "on_demand",
        "playlist_metadata": {"name": "News"},
        "epg": {"mode": "manual", "url": "https://example.test/guide.xml", "channel_id": "42"},
        "events": {"automation": {"armed": true,
            "on_break_start": {"action": "swap_cell", "target": "s"}}}}),
    );
    assert!(base.same_media(&edited));
    assert!(edited.same_media(&base));
}

#[test]
fn media_fields_still_differ() {
    let base = source(
        json!({"id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
        "playlist_metadata": {"name": "News"}}),
    );
    let moved = source(
        json!({"id": "w", "kind": "hls", "url": "https://example.test/other.m3u8",
        "playlist_metadata": {"name": "News"}}),
    );
    assert!(!base.same_media(&moved), "url is media");
    let unobserved = source(
        json!({"id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
        "events": {"observe": false}}),
    );
    assert!(!base.same_media(&unobserved), "observe is media");
    let renamed = source(
        json!({"id": "w", "kind": "hls", "url": "https://example.test/w.m3u8",
        "display_name": "Other"}),
    );
    assert!(
        !base.same_media(&renamed),
        "display_name keeps the canonical path"
    );
}
