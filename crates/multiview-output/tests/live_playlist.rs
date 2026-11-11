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
fn query_fragment_and_encoded_sibling_uris_pin_exact_retired_segments() {
    for (uri, pinned) in [
        ("video-0.ts?token=x", "video-0.ts"),
        ("video-0.ts#fragment", "video-0.ts"),
        ("video%2D0.ts?token=x#fragment", "video-0.ts"),
        ("video%20old.ts", "video old.ts"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("audio-program-g0.m3u8"),
            format!("#EXTM3U\n{uri}\n"),
        )
        .unwrap();
        let mut live = LivePlaylist::new(dir.path().join("video-g0.m3u8"), 2);
        for index in 0..10 {
            let name = if index == 0 {
                pinned.to_owned()
            } else {
                format!("video-{index}.ts")
            };
            let path = touch_segment(dir.path(), &name);
            live.push_closed_segment(name, path, 1.0, index * 1_000_000_000)
                .unwrap();
        }
        assert!(dir.path().join(pinned).exists(), "URI {uri} lost {pinned}");
        assert!(
            !dir.path().join("video-1.ts").exists(),
            "URI {uri} blocked safe GC"
        );
    }
}

#[test]
fn malformed_sibling_uri_cannot_traverse_or_authorize_pruning() {
    let dir = tempfile::tempdir().unwrap();
    for uri in [
        "../video-0.ts",
        "/video-0.ts",
        "https://example.test/video-0.ts",
        "video%2F0.ts",
        "video%252F0.ts",
        "%2E%2E%2Fvideo-0.ts",
        "video-%ZZ.ts",
        "video-0.ts?token=%ZZ",
        "video-0.ts#fragment%ZZ",
    ] {
        std::fs::write(
            dir.path().join("audio-program-g0.m3u8"),
            format!("#EXTM3U\n{uri}\n"),
        )
        .unwrap();
        let mut live = LivePlaylist::new(dir.path().join("video-g0.m3u8"), 2);
        for index in 0..10 {
            let name = format!("video-{index}.ts");
            let path = touch_segment(dir.path(), &name);
            live.push_closed_segment(name, path, 1.0, index * 1_000_000_000)
                .unwrap();
        }
        assert!(
            dir.path().join("video-0.ts").is_file(),
            "URI {uri} allowed GC"
        );
    }
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

/// #220: with background publication, a slow disk (every manifest write takes
/// 300 ms, like the measured fsync tail) must never block the segment close on
/// the sink's packet path; the newest manifest still lands and pruning keeps the
/// synchronous retention rule.
#[test]
fn background_publish_never_blocks_on_a_slow_manifest_write() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const WINDOW: usize = 3;
    const TOTAL: usize = 12;
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("index.m3u8");
    let mut live = LivePlaylist::new(manifest.clone(), WINDOW);
    let slow: multiview_output::hls::ManifestWriter =
        Arc::new(|path, bytes, fence: &multiview_output::hls::PublishFence| {
            std::thread::sleep(Duration::from_millis(300));
            fence
                .commit(&mut || {
                    std::fs::write(path, bytes)
                        .map_err(|e| multiview_output::Error::Output(e.to_string()))
                })
                .map(|_| ())
        });
    live.enable_background_publish_with(slow)
        .expect("publisher");

    let mut seg_paths = Vec::new();
    let mut worst = Duration::ZERO;
    for i in 0..TOTAL {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        seg_paths.push(path.clone());
        let started = Instant::now();
        live.push_closed_segment(name, path, 2.0, i64::try_from(i).unwrap() * 2_000_000_000)
            .expect("close never fails here");
        worst = worst.max(started.elapsed());
    }
    assert!(
        worst < Duration::from_millis(50),
        "a segment close waited {worst:?} on the manifest write (inv #10)"
    );

    live.finalize().expect("final publish");
    let text = std::fs::read_to_string(&manifest).expect("manifest");
    assert!(
        text.contains("#EXT-X-ENDLIST"),
        "finalize publishes the last manifest"
    );
    let listed: Vec<&str> = text
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        listed,
        ["seg9.ts", "seg10.ts", "seg11.ts"],
        "newest window listed"
    );
    for uri in &listed {
        assert!(dir.path().join(uri).is_file(), "listed {uri} must exist");
    }
    // Retention: the listed window plus one further grace window (>= 3) stay; older
    // segments that were handed to a successful publish are pruned.
    for (i, path) in seg_paths.iter().enumerate() {
        let keep = i + 2 * WINDOW.max(3) >= TOTAL;
        if keep {
            assert!(
                path.exists(),
                "seg{i} is within listed + grace and must remain"
            );
        }
    }
    assert!(
        seg_paths
            .iter()
            .take(TOTAL - 2 * WINDOW.max(3))
            .any(|p| !p.exists()),
        "segments older than listed + grace are pruned once a publish succeeds"
    );
}

/// #220: a failing background publish must surface on a later call and must not
/// prune anything a previously published manifest may still list.
#[test]
fn background_publish_failure_surfaces_and_prunes_nothing() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("index.m3u8");
    let mut live = LivePlaylist::new(manifest, 1);
    let failing = Arc::new(AtomicBool::new(true));
    let gate = Arc::clone(&failing);
    let writer: multiview_output::hls::ManifestWriter = Arc::new(
        move |_path, _bytes, _fence: &multiview_output::hls::PublishFence| {
            if gate.load(Ordering::SeqCst) {
                Err(multiview_output::Error::Output(
                    "injected disk failure".to_owned(),
                ))
            } else {
                Ok(())
            }
        },
    );
    live.enable_background_publish_with(writer)
        .expect("publisher");

    let mut paths = Vec::new();
    let mut surfaced = false;
    for i in 0..10 {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        paths.push(path.clone());
        if live
            .push_closed_segment(name, path, 2.0, i64::from(i) * 2_000_000_000)
            .is_err()
        {
            surfaced = true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        surfaced,
        "a failed background publish must surface to the caller"
    );
    for (i, path) in paths.iter().enumerate() {
        assert!(
            path.exists(),
            "seg{i} must not be pruned while publishes fail"
        );
    }
    assert!(
        live.finalize().is_err(),
        "finalize reports the failed final publish"
    );
}

/// #220 review: after a failed background publish recovers, the first successful
/// manifest must still list every URI the failed manifest listed (the failed one
/// is retried, the close that surfaced the error is not committed).
#[test]
fn background_publish_recovers_without_skipping_the_failed_manifest() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("index.m3u8");
    let mut live = LivePlaylist::new(manifest.clone(), 2);
    let calls = Arc::new(AtomicUsize::new(0));
    let written: Arc<Mutex<Vec<String>>> = Arc::default();
    let (c, w) = (Arc::clone(&calls), Arc::clone(&written));
    let writer: multiview_output::hls::ManifestWriter = Arc::new(
        move |path, bytes, fence: &multiview_output::hls::PublishFence| {
            // The second publish fails once; every other publish succeeds.
            if c.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err(multiview_output::Error::Output("injected".to_owned()));
            }
            w.lock()
                .unwrap()
                .push(String::from_utf8_lossy(bytes).into_owned());
            fence
                .commit(&mut || {
                    std::fs::write(path, bytes)
                        .map_err(|e| multiview_output::Error::Output(e.to_string()))
                })
                .map(|_| ())
        },
    );
    live.enable_background_publish_with(writer)
        .expect("publisher");

    let mut failed_manifest_uris: Option<Vec<String>> = None;
    for i in 0..8 {
        let name = format!("seg{i}.ts");
        let path = touch_segment(dir.path(), &name);
        // Let the publisher drain so the injected failure is observed in order.
        std::thread::sleep(std::time::Duration::from_millis(30));
        if live
            .push_closed_segment(name, path, 2.0, i64::from(i) * 2_000_000_000)
            .is_err()
        {
            // The manifest that failed was the one committed before this close.
            failed_manifest_uris =
                Some((0..i).rev().take(2).map(|n| format!("seg{n}.ts")).collect());
        }
    }
    live.finalize().expect("final publish");
    let failed = failed_manifest_uris.expect("the injected failure must surface");
    let published = written.lock().unwrap().clone();
    let first_after_failure = published
        .get(1)
        .expect("a successful publish follows the failure");
    for uri in &failed {
        assert!(
            first_after_failure.contains(uri.as_str()),
            "first publish after recovery must still list {uri}: {first_after_failure}"
        );
    }
}

/// #220 review: dropping a background-published playlist (an errored sink) must
/// not leave a writer that lands on the manifest path afterwards and races the
/// next run's publication of the same file.
#[test]
fn dropping_a_background_playlist_quiesces_its_writer() {
    use std::sync::Arc;
    use std::time::Duration;

    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("index.m3u8");
    let mut live = LivePlaylist::new(manifest.clone(), 2);
    // A slow write that, like the production writer, checks the fence right
    // before making the manifest visible.
    let slow: multiview_output::hls::ManifestWriter =
        Arc::new(|path, bytes, fence: &multiview_output::hls::PublishFence| {
            std::thread::sleep(Duration::from_millis(200));
            fence
                .commit(&mut || {
                    std::fs::write(path, bytes)
                        .map_err(|e| multiview_output::Error::Output(e.to_string()))
                })
                .map(|_| ())
        });
    live.enable_background_publish_with(slow)
        .expect("publisher");
    let path = touch_segment(dir.path(), "seg0.ts");
    live.push_closed_segment("seg0.ts", path, 2.0, 0)
        .expect("close");
    let dropped = std::time::Instant::now();
    drop(live);
    assert!(
        dropped.elapsed() < Duration::from_millis(50),
        "dropping must not wait for the in-flight write (packet thread)"
    );
    // The next run publishes the same path immediately after the drop.
    std::fs::write(&manifest, b"#EXTM3U\n#NEXT-RUN\n").expect("next run");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        "#EXTM3U\n#NEXT-RUN\n",
        "a dropped run's publisher wrote after it was dropped"
    );
}
