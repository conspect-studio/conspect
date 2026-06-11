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

/// Feeding N > window segment closes into the rolling driver must leave an
/// on-disk playlist that lists exactly `window` segments, advances
/// `EXT-X-MEDIA-SEQUENCE` to `N - window`, carries NO `EXT-X-ENDLIST` while the
/// run is live, and deletes the evicted `.ts` files from disk.
#[test]
fn rolling_live_playlist_windows_publishes_and_prunes() {
    const WINDOW: usize = 6;
    const TOTAL: usize = 10;

    let dir = tempfile::tempdir().expect("tempdir");
    let playlist_path = dir.path().join("multiview.m3u8");

    let mut live = LivePlaylist::new(playlist_path.clone(), WINDOW);

    let mut seg_paths = Vec::with_capacity(TOTAL);
    for i in 0..TOTAL {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        seg_paths.push(path.clone());
        live.push_closed_segment(name, path, 2.0)
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
    // The window holds the most recent WINDOW segments: seg4.ts..seg9.ts.
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

    // Evicted `.ts` files (the first N - window) must be deleted from disk; the
    // windowed ones must remain.
    for (i, path) in seg_paths.iter().enumerate() {
        if i < TOTAL - WINDOW {
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

    for i in 0..3 {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        live.push_closed_segment(name, path, 2.0)
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
