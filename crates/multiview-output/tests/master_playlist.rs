//! Golden-string integration tests for the HLS master (multivariant) playlist.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_output::hls::{MasterPlaylist, VariantStream};

/// A master playlist lists each variant's `#EXT-X-STREAM-INF` with bandwidth,
/// codecs, and resolution, followed by the variant URI line.
#[test]
fn master_playlist_golden() {
    let mut master = MasterPlaylist::new();
    master.push_variant(
        VariantStream::new("v720.m3u8", 3_000_000)
            .with_codecs("avc1.640028,mp4a.40.2")
            .with_resolution(1280, 720)
            .with_frame_rate(30.0),
    );
    master.push_variant(
        VariantStream::new("v1080.m3u8", 6_000_000)
            .with_codecs("avc1.640028,mp4a.40.2")
            .with_resolution(1920, 1080)
            .with_frame_rate(30.0),
    );

    let expected = "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=3000000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1280x720,FRAME-RATE=30.000
v720.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=6000000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,FRAME-RATE=30.000
v1080.m3u8
";
    assert_eq!(master.render(), expected);
}

/// A minimal variant with only bandwidth still renders a valid stream-inf line.
#[test]
fn minimal_variant_golden() {
    let mut master = MasterPlaylist::new();
    master.push_variant(VariantStream::new("only.m3u8", 1_000_000));

    let expected = "\
#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=1000000
only.m3u8
";
    assert_eq!(master.render(), expected);
}

/// Average bandwidth, when supplied, is emitted alongside peak bandwidth.
#[test]
fn variant_with_average_bandwidth() {
    let mut master = MasterPlaylist::new();
    master.push_variant(
        VariantStream::new("v.m3u8", 6_000_000)
            .with_average_bandwidth(4_500_000)
            .with_resolution(1920, 1080),
    );
    let out = master.render();
    assert!(
        out.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=6000000,AVERAGE-BANDWIDTH=4500000,RESOLUTION=1920x1080\n"
        ),
        "{out}"
    );
}
