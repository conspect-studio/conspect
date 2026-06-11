//! RT-1 acceptance tests for the `StreamInventory` model (ADR-0034 §3).
//!
//! Pure model: the typed inventory record, its kind helpers, the kind-specific
//! detail, and the serde shape. The libav / TS / HLS *discovery* of this model
//! is tested in `multiview-ffmpeg` / `multiview-input` behind the `ffmpeg`
//! feature; here we test the model in isolation (default build, no native deps).
//!
//! Integration tests do not inherit `clippy.toml`'s test relaxations, so this
//! file opts out of the panic-bearing lints explicitly (CLAUDE.md §A.1).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::stream::{
    Bcp47, DataKind, StableStreamId, StreamDescriptor, StreamDetail, StreamInventory, StreamKind,
    TcSourceKind,
};
use multiview_core::time::Rational;

/// A small fixture inventory: 1 video, 2 audio (eng/spa), 1 subtitle (fra),
/// 1 SCTE-35 data stream, 1 timecode stream — one of every routing kind.
fn fixture() -> StreamInventory {
    let eng = Bcp47::try_from("eng").unwrap();
    let spa = Bcp47::try_from("spa").unwrap();
    let fra = Bcp47::try_from("fra").unwrap();

    let video = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Video, 0, "h264", None, None),
        StreamKind::Video,
        "h264",
        StreamDetail::Video {
            width: 1920,
            height: 1080,
            frame_rate: Some(Rational::FPS_25),
        },
    )
    .with_default(true);

    let audio0 = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Audio, 0, "aac", Some(&eng), None),
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels: 2,
            sample_rate: 48_000,
        },
    )
    .with_language(Some(eng))
    .with_default(true);

    let audio1 = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Audio, 1, "aac", Some(&spa), None),
        StreamKind::Audio,
        "aac",
        StreamDetail::Audio {
            channels: 2,
            sample_rate: 48_000,
        },
    )
    .with_language(Some(spa));

    let subtitle = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Subtitle, 0, "subrip", Some(&fra), None),
        StreamKind::Subtitle,
        "subrip",
        StreamDetail::Subtitle { forced: false },
    )
    .with_language(Some(fra));

    let scte = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Data(DataKind::Scte35), 0, "scte_35", None, None),
        StreamKind::Data(DataKind::Scte35),
        "scte_35",
        StreamDetail::Passthrough,
    );

    let tc = StreamDescriptor::new(
        StableStreamId::from_general(
            StreamKind::Timecode(TcSourceKind::AtcRp188),
            0,
            "timed_id3",
            None,
            None,
        ),
        StreamKind::Timecode(TcSourceKind::AtcRp188),
        "timed_id3",
        StreamDetail::Passthrough,
    );

    StreamInventory::from_streams(vec![video, audio0, audio1, subtitle, scte, tc])
        .with_input_id("cam-a")
}

#[test]
fn by_kind_partitions_streams_by_routing_kind() {
    let inv = fixture();
    assert_eq!(inv.video().count(), 1, "one video stream");
    assert_eq!(inv.audio_tracks().count(), 2, "two audio tracks");
    assert_eq!(inv.subtitle_tracks().count(), 1, "one subtitle track");
    assert_eq!(inv.data().count(), 1, "one data stream");
    assert_eq!(inv.timecode().count(), 1, "one timecode stream");

    // by_kind takes a predicate so an exact data kind is addressable.
    let scte: Vec<_> = inv
        .by_kind(|k| k == StreamKind::Data(DataKind::Scte35))
        .collect();
    assert_eq!(scte.len(), 1, "exactly one SCTE-35 stream by exact kind");
    assert!(inv
        .by_kind(|k| k == StreamKind::Data(DataKind::Klv))
        .next()
        .is_none());
}

#[test]
fn audio_tracks_preserve_language_and_layout() {
    let inv = fixture();
    let langs: Vec<&str> = inv
        .audio_tracks()
        .filter_map(|s| s.language.as_ref())
        .map(Bcp47::as_str)
        .collect();
    assert_eq!(langs, vec!["eng", "spa"], "both audio languages survive");

    for a in inv.audio_tracks() {
        assert_eq!(
            a.detail.audio_layout(),
            Some((2, 48_000)),
            "audio layout carried in detail"
        );
    }
}

#[test]
fn video_descriptor_carries_geometry_detail() {
    let inv = fixture();
    let v = inv.video().next().expect("a video stream");
    assert_eq!(v.detail.video_geometry(), Some((1920, 1080)));
    if let StreamDetail::Video { frame_rate, .. } = v.detail {
        assert_eq!(frame_rate, Some(Rational::FPS_25));
    } else {
        panic!("video detail must be StreamDetail::Video");
    }
}

#[test]
fn default_for_prefers_flagged_then_falls_back_to_first() {
    let inv = fixture();

    // audio0 is flagged default → it is chosen even though audio1 also exists.
    let a = inv
        .default_for(StreamKind::is_audio)
        .expect("a default audio");
    assert_eq!(
        a.language.as_ref().map(Bcp47::as_str),
        Some("eng"),
        "flagged-default audio wins"
    );

    // Subtitle has no flagged default → the first subtitle is the fallback.
    let s = inv
        .default_for(StreamKind::is_subtitle)
        .expect("a default subtitle");
    assert_eq!(s.language.as_ref().map(Bcp47::as_str), Some("fra"));

    // No video-of-a-nonexistent-kind: a predicate matching nothing → None.
    assert!(inv
        .default_for(|k| k == StreamKind::Data(DataKind::Klv))
        .is_none());
}

#[test]
fn default_for_video_falls_back_when_unflagged() {
    // An inventory whose only video is NOT flagged default still resolves to it.
    let video = StreamDescriptor::new(
        StableStreamId::from_general(StreamKind::Video, 0, "h264", None, None),
        StreamKind::Video,
        "h264",
        StreamDetail::Video {
            width: 640,
            height: 480,
            frame_rate: None,
        },
    );
    let inv = StreamInventory::from_streams(vec![video]);
    assert!(
        inv.default_for(StreamKind::is_video).is_some(),
        "first-of-kind fallback when nothing is flagged default"
    );
}

#[test]
fn inventory_serde_round_trips_with_tagged_kind_and_detail() {
    let inv = fixture();
    let json = serde_json::to_string(&inv).expect("serialise inventory");
    // The wire form must never be `untagged`: the kind tag + detail tag must be
    // present and unambiguous.
    assert!(
        json.contains("\"kind\":\"video\""),
        "video kind tag present"
    );
    assert!(
        json.contains("\"detail\":\"video\""),
        "video detail tag present"
    );
    assert!(
        json.contains("\"kind\":\"data\"") && json.contains("\"payload\":\"scte35\""),
        "SCTE-35 data kind carries its payload tag"
    );

    let back: StreamInventory = serde_json::from_str(&json).expect("deserialise inventory");
    assert_eq!(back, inv, "inventory round-trips losslessly");
    assert_eq!(back.input_id.as_deref(), Some("cam-a"));
}

#[test]
fn empty_inventory_helpers_are_total() {
    let inv = StreamInventory::new();
    assert_eq!(inv.video().count(), 0);
    assert_eq!(inv.audio_tracks().count(), 0);
    assert!(inv.default_for(StreamKind::is_video).is_none());
    assert!(inv.input_id.is_none());
}

#[test]
fn other_codec_rows_classify_into_data_or_timecode_descriptors() {
    use multiview_core::stream::CoarseMediaKind;

    // A real in-container SCTE-35 / KLV fixture is infeasible via the LGPL CLI
    // (the demux test cannot mux one), so the `kind = Other` → routing-kind
    // refinement that the libav inventory path applies is covered here as a pure
    // unit test over the same classifier (`from_coarse_and_codec`, RT-0) feeding
    // an RT-1 descriptor with passthrough detail.
    let cases = [
        ("scte_35", StreamKind::Data(DataKind::Scte35)),
        ("smpte_klv", StreamKind::Data(DataKind::Klv)),
        ("klv", StreamKind::Data(DataKind::Klv)),
        ("timed_id3", StreamKind::Timecode(TcSourceKind::AtcRp188)),
        // Unknown data essence stays routable as a generic Data passthrough,
        // never dropped (the as-built `best_stream` returned None for Other).
        ("bin_data", StreamKind::Data(DataKind::Klv)),
    ];
    for (codec, want_kind) in cases {
        let kind = StreamKind::from_coarse_and_codec(CoarseMediaKind::Other, codec);
        assert_eq!(kind, want_kind, "codec {codec:?} classifies wrong");

        let desc = StreamDescriptor::new(
            StableStreamId::from_general(kind, 0, codec, None, None),
            kind,
            codec,
            StreamDetail::Passthrough,
        );
        // A non-AV passthrough descriptor carries no AV detail.
        assert_eq!(desc.detail, StreamDetail::Passthrough);
        assert!(desc.detail.video_geometry().is_none());
        assert!(desc.detail.audio_layout().is_none());

        // And it lands in the right inventory bucket (data vs timecode).
        let inv = StreamInventory::from_streams(vec![desc]);
        if want_kind.is_timecode() {
            assert_eq!(inv.timecode().count(), 1);
            assert_eq!(inv.data().count(), 0);
        } else {
            assert_eq!(inv.data().count(), 1);
            assert_eq!(inv.timecode().count(), 0);
        }
    }
}
