//! HLS-0/HLS-1 rolling-live-playlist driver tests (ADR-0032).
//!
//! A live multiview run is infinite: the playlist must be (re)written on disk on
//! every closed segment with a **bounded** segment window, never once-at-finalize.
//! These tests drive the pure-Rust [`LivePlaylist`] driver directly (no encoder
//! needed): they create dummy `.ts` files on disk, feed more segment closes than
//! the window holds, and assert the on-disk `.m3u8` rolls correctly, that evicted
//! `.ts` files are pruned from disk, that no `#EXT-X-ENDLIST` appears while live,
//! and that finalize appends it. This pins the behaviour the live demo's HLS
//! playback depends on.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;

use multiview_output::hls::LivePlaylist;

/// Write a dummy segment file on disk so the driver has something to prune, and
/// return its path. The contents are irrelevant — the driver only renames/unlinks
/// whole files.
fn touch_segment(dir: &std::path::Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"dummy-ts-bytes").expect("write dummy segment");
    path
}

#[test]
fn sibling_manifest_pins_retired_segment_until_sibling_retires() {
    let dir = tempfile::tempdir().unwrap();
    let sibling = dir.path().join("audio-program-g0.m3u8");
    std::fs::write(&sibling, "#EXTM3U\nvideo-0.ts\n").unwrap();
    let mut live = LivePlaylist::new(dir.path().join("video-g0.m3u8"), 2);
    for index in 0..10 {
        let name = format!("video-{index}.ts");
        let path = touch_segment(dir.path(), &name);
        live.push_closed_segment(name, path, 1.0, index * 1_000_000_000)
            .unwrap();
    }
    assert!(dir.path().join("video-0.ts").is_file());
    assert!(!dir.path().join("video-1.ts").exists());
    // The sink's sequence reaper collects this deferred TS after retirement.
    // The local driver must never unlink it while the sibling references it.
    assert!(sibling.exists());
}

#[test]
fn malformed_sibling_uri_cannot_traverse_or_authorize_pruning() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("audio-program-g0.m3u8"),
        "#EXTM3U\n../video-0.ts\n",
    )
    .unwrap();
    let mut live = LivePlaylist::new(dir.path().join("video-g0.m3u8"), 2);
    for index in 0..10 {
        let name = format!("video-{index}.ts");
        let path = touch_segment(dir.path(), &name);
        live.push_closed_segment(name, path, 1.0, index * 1_000_000_000)
            .unwrap();
    }
    assert!(dir.path().join("video-0.ts").is_file());
}

#[test]
fn failed_publish_preserves_previous_manifest_and_all_its_segments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("index.m3u8");
    let mut live = LivePlaylist::new(manifest.clone(), 2);
    for index in 0..4 {
        let name = format!("seg{index}.ts");
        let path = touch_segment(dir.path(), &name);
        live.push_closed_segment(name, path, 2.0, index * 2_000_000_000)
            .expect("publish");
    }
    let original = std::fs::read_to_string(&manifest).expect("original manifest");
    // The same-directory temp name is a directory: File::create fails before rename.
    std::fs::create_dir(manifest.with_file_name("index.m3u8.tmp")).expect("inject failure");
    let next = touch_segment(dir.path(), "seg4.ts");
    assert!(live
        .push_closed_segment("seg4.ts", next, 2.0, 8_000_000_000)
        .is_err());
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), original);
    for uri in original
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        assert!(
            dir.path().join(uri).is_file(),
            "published URI {uri} must exist"
        );
    }
    std::fs::remove_dir(manifest.with_file_name("index.m3u8.tmp")).unwrap();
    let next = touch_segment(dir.path(), "seg5.ts");
    live.push_closed_segment("seg5.ts", next, 2.0, 10_000_000_000)
        .expect("publish after recovery");
    let current = std::fs::read_to_string(&manifest).unwrap();
    assert!(current.contains("seg5.ts"));
    assert!(!current.contains("seg4.ts"));
    for uri in current
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        assert!(
            dir.path().join(uri).is_file(),
            "published URI {uri} must exist"
        );
    }
}

#[test]
fn restarted_audio_and_video_playlists_advance_together_with_discontinuity() {
    let dir = tempfile::tempdir().expect("tempdir");
    for name in ["index.m3u8", "audio-program.m3u8", "audio-track0.m3u8"] {
        let mut playlist = LivePlaylist::new(dir.path().join(name), 2);
        playlist.set_media_sequence(17);
        playlist.set_initial_discontinuity();
        for index in 17..20 {
            let segment = touch_segment(dir.path(), &format!("{name}-{index}.ts"));
            playlist
                .push_closed_segment(
                    segment.file_name().unwrap().to_string_lossy(),
                    segment.clone(),
                    1.0,
                    index * 1_000_000_000,
                )
                .expect("publish");
        }
        let text = std::fs::read_to_string(dir.path().join(name)).expect("manifest");
        assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:18"), "{text}");
        assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:1"), "{text}");
        assert!(
            dir.path().join(format!("{name}-17.ts")).exists(),
            "client grace keeps the old segment"
        );
    }
}

/// Feeding N > window segment closes into the rolling driver must leave an
/// on-disk playlist that lists exactly `window` segments, advances
/// `EXT-X-MEDIA-SEQUENCE` to `N - window`, carries NO `EXT-X-ENDLIST` while the
/// run is live, and deletes the evicted `.ts` files from disk.
#[test]
fn rolling_live_playlist_windows_publishes_and_prunes() {
    const WINDOW: usize = 6;
    const TOTAL: usize = 16;

    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");

    let mut live = LivePlaylist::new(playlist_path.clone(), WINDOW);

    let mut seg_paths = Vec::with_capacity(TOTAL);
    for i in 0..TOTAL {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        seg_paths.push(path.clone());
        let start_pts_ns = i64::try_from(i).unwrap_or(0) * 2_000_000_000;
        live.push_closed_segment(name, path, 2.0, start_pts_ns)
            .expect("publish closed segment");
    }

    // The playlist was written on EACH close (rolling), not once at finalize: it
    // exists on disk now, mid-run, with no ENDLIST.
    assert!(
        playlist_path.exists(),
        "the live playlist must be written to disk on each closed segment, not only at finalize"
    );
    let text = std::fs::read_to_string(&playlist_path).expect("read playlist");

    // Exactly `window` segments listed (the most recent ones). A segment URI is a
    // non-empty, non-tag line (every directive starts with `#`).
    let listed: Vec<&str> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        listed.len(),
        WINDOW,
        "the live playlist must list exactly the window size, got {listed:?}"
    );
    // The window holds the most recent WINDOW segments.
    let expected: Vec<String> = (TOTAL - WINDOW..TOTAL)
        .map(|i| format!("seg{i}.ts"))
        .collect();
    assert_eq!(
        listed,
        expected.iter().map(String::as_str).collect::<Vec<_>>(),
        "the window must hold the most recent segments in order"
    );

    // EXT-X-MEDIA-SEQUENCE advanced to N - window as the oldest were evicted.
    assert!(
        text.contains(&format!("#EXT-X-MEDIA-SEQUENCE:{}", TOTAL - WINDOW)),
        "EXT-X-MEDIA-SEQUENCE must advance to N-window = {}; playlist was:\n{text}",
        TOTAL - WINDOW
    );

    // No ENDLIST while the run is live.
    assert!(
        !text.contains("#EXT-X-ENDLIST"),
        "a live (unfinished) playlist must NOT carry #EXT-X-ENDLIST; playlist was:\n{text}"
    );

    // A complete previous window remains available for clients that hold its
    // manifest; older segments expire, keeping disk bounded to two windows.
    for (i, path) in seg_paths.iter().enumerate() {
        if i < TOTAL - 2 * WINDOW {
            assert!(
                !path.exists(),
                "evicted segment {} must be deleted from disk (bounded disk)",
                path.display()
            );
        } else {
            assert!(
                path.exists(),
                "an in-window segment {} must remain on disk",
                path.display()
            );
        }
    }
}

/// After finalize, the on-disk playlist gains `#EXT-X-ENDLIST` (the run ended).
#[test]
fn finalize_appends_endlist() {
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");
    let mut live = LivePlaylist::new(playlist_path.clone(), 6);

    for i in 0..3i64 {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        let start_pts_ns = i * 2_000_000_000;
        live.push_closed_segment(name, path, 2.0, start_pts_ns)
            .expect("publish closed segment");
    }
    // Mid-run: no ENDLIST.
    let mid = std::fs::read_to_string(&playlist_path).expect("read mid playlist");
    assert!(
        !mid.contains("#EXT-X-ENDLIST"),
        "mid-run playlist has no ENDLIST"
    );

    live.finalize().expect("finalize live playlist");
    let end = std::fs::read_to_string(&playlist_path).expect("read final playlist");
    assert!(
        end.contains("#EXT-X-ENDLIST"),
        "the finalized playlist must carry #EXT-X-ENDLIST; playlist was:\n{end}"
    );
}
