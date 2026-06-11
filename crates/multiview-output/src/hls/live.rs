//! HLS-0/HLS-1 rolling **live** playlist driver (ADR-0032).
//!
//! A live multiview run never finalizes: it produces segments forever. The batch
//! [`SegmentSink`](crate::sink::SegmentSink) model (accumulate every segment in a
//! `Vec`, render the playlist once at the end) therefore never writes the
//! `.m3u8` and grows the segment set without bound. [`LivePlaylist`] is the
//! rolling driver that fixes both: on **each** closed segment it pushes the
//! segment into a windowed [`MediaPlaylist`], re-renders, and publishes the
//! manifest to disk **atomically** (same-dir `.tmp` → fsync → `rename(2)`), then
//! prunes the `.ts` file evicted from the window so disk stays bounded.
//!
//! This module is **pure Rust** — it performs only filesystem I/O and drives the
//! pure-text [`MediaPlaylist`] generator; it pulls no native dependency and is
//! compiled in every build (the encoder-fed segment muxing that feeds it lives
//! behind the `ffmpeg` feature). It upholds invariants #1/#10 by construction:
//! it runs entirely on the off-hot-path egress thread and never blocks the output
//! clock or back-pressures the engine.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{MediaPlaylist, Segment, SegmentType};
use crate::error::{Error, Result};

/// The rolling live-playlist driver: a windowed [`MediaPlaylist`] plus the
/// on-disk publication path and the in-window segment files (so the evicted ones
/// can be pruned).
///
/// Construct with [`LivePlaylist::new`], call [`push_closed_segment`] on **each**
/// GOP-aligned segment as it is closed, and [`finalize`] once at end-of-run (the
/// only time `#EXT-X-ENDLIST` is emitted). While the run is live the rendered
/// manifest omits the end marker, so a player keeps reloading at the live edge.
///
/// [`push_closed_segment`]: LivePlaylist::push_closed_segment
/// [`finalize`]: LivePlaylist::finalize
#[derive(Debug)]
pub struct LivePlaylist {
    /// Where the rendered `.m3u8` is published (atomically) on every close.
    playlist_path: PathBuf,
    /// The bounded segment window (number of segments kept in the playlist and on
    /// disk).
    window: usize,
    /// The windowed media playlist; `set_window(window)` is applied at
    /// construction so [`MediaPlaylist::push_segment`] evicts + advances the media
    /// (and discontinuity) sequence per RFC 8216bis §6.2.2.
    playlist: MediaPlaylist,
    /// The on-disk paths of the segments currently inside the window, oldest at
    /// the front. When the count exceeds `window` the front path is popped and
    /// the corresponding `.ts` file unlinked (best-effort) so disk stays bounded.
    seg_paths: VecDeque<PathBuf>,
}

impl LivePlaylist {
    /// Create a rolling driver publishing to `playlist_path`, keeping at most
    /// `window` segments in the playlist (and on disk). MPEG-TS segments (no
    /// init segment); fMP4/CMAF is a later slice (HLS-6/7).
    #[must_use]
    pub fn new(playlist_path: PathBuf, window: usize) -> Self {
        let mut playlist = MediaPlaylist::new(SegmentType::MpegTs);
        playlist.set_window(window);
        Self {
            playlist_path,
            window,
            playlist,
            seg_paths: VecDeque::new(),
        }
    }

    /// The on-disk path the manifest is published to.
    #[must_use]
    pub fn playlist_path(&self) -> &Path {
        &self.playlist_path
    }

    /// Record one **closed** GOP-aligned segment: append it to the windowed
    /// playlist (`uri` referenced relative to the manifest, `duration` its
    /// `EXTINF` seconds), recompute `EXT-X-TARGETDURATION`, and atomically
    /// re-publish the manifest to disk. The segment's on-disk `path` is tracked so
    /// that, once it ages out of the window, its `.ts` file is pruned.
    ///
    /// The published manifest carries **no** `#EXT-X-ENDLIST` (the run is live);
    /// [`finalize`](Self::finalize) adds it once at end-of-run.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the manifest could not be atomically written
    /// (the temp write, fsync, or rename failed). Pruning an aged-out segment is
    /// best-effort and never surfaces an error (a one-refresh-behind client 404 is
    /// acceptable; a missing file is ignored).
    pub fn push_closed_segment(
        &mut self,
        uri: impl Into<String>,
        path: PathBuf,
        duration: f64,
    ) -> Result<()> {
        // Append to the windowed playlist; `push_segment` auto-evicts the oldest
        // beyond the window and advances EXT-X-MEDIA-SEQUENCE / -DISCONTINUITY-
        // SEQUENCE (RFC 8216bis §6.2.2) — we never touch those counters by hand.
        self.playlist.push_segment(Segment::new(uri, duration));
        // TARGETDURATION must be >= every EXTINF still listed, as an integer.
        self.playlist.recompute_target_duration();

        // Track the new segment file and prune any that just aged out of the
        // window so disk stays bounded (the playlist already dropped them).
        self.seg_paths.push_back(path);
        while self.seg_paths.len() > self.window {
            if let Some(evicted) = self.seg_paths.pop_front() {
                prune_segment(&evicted);
            }
        }

        // Atomically publish the rolling manifest (no ENDLIST while live).
        self.publish()
    }

    /// Finalize the live playlist at end-of-run: mark it finished
    /// (`#EXT-X-ENDLIST`) and publish the final manifest atomically. After this a
    /// player sees a terminated VOD-style playlist of the last window of segments.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the final manifest could not be atomically
    /// written.
    pub fn finalize(&mut self) -> Result<()> {
        self.playlist.set_finished(true);
        self.publish()
    }

    /// Render the current playlist and atomically publish it to `playlist_path`.
    fn publish(&self) -> Result<()> {
        let text = self.playlist.render();
        atomic_write(&self.playlist_path, text.as_bytes())
    }
}

/// Best-effort unlink of an evicted segment file. The result is deliberately
/// discarded: a `NotFound` means the file is already gone, and any other unlink
/// error is swallowed rather than failing the live run — bounding disk is
/// best-effort here (a one-refresh-behind client 404 is acceptable, and a proper
/// grace-period reaper is HLS-2-full). The deliberate non-propagation keeps a
/// transient filesystem hiccup from stalling the rolling publish (invariant #1);
/// a failed unlink merely leaves the file lingering until process exit.
fn prune_segment(path: &Path) {
    // `Ok` and every `Err` (including `NotFound`) are no-ops here; the explicit
    // `match` makes the deliberate drop clear rather than a silent `let _ =`.
    match std::fs::remove_file(path) {
        Ok(()) | Err(_) => {}
    }
}

/// Atomically write `bytes` to `path`: write to a same-directory `<name>.tmp`,
/// fsync the file (so its contents are durable before it is exposed under the
/// final name), then `rename(2)` it into place. The same-directory temp avoids
/// `EXDEV` (cross-filesystem rename), and fsync-before-rename closes the
/// crash-truncation window a fronting nginx/CDN could otherwise serve.
///
/// # Errors
/// Returns [`Error::Output`] if the temp file cannot be created/written/synced or
/// the rename fails.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(path);
    // Scope the file handle so it is flushed/closed before the rename.
    {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| Error::Output(format!("creating temp file {}: {e}", tmp.display())))?;
        file.write_all(bytes)
            .map_err(|e| Error::Output(format!("writing temp file {}: {e}", tmp.display())))?;
        // fsync the contents so a crash after the rename cannot expose a
        // truncated/empty manifest under the final name.
        file.sync_all()
            .map_err(|e| Error::Output(format!("fsync temp file {}: {e}", tmp.display())))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup of the temp on a failed rename so it does not leak.
        match std::fs::remove_file(&tmp) {
            Ok(()) | Err(_) => {}
        }
        Error::Output(format!(
            "renaming {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
}

/// Derive the same-directory temp path for `path` by appending a `.tmp` suffix to
/// its file name (e.g. `multiview.m3u8` -> `multiview.m3u8.tmp`). Keeping the temp
/// in the same directory is what makes the subsequent `rename(2)` atomic and
/// `EXDEV`-free.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map_or_else(
        || std::ffi::OsString::from("hls-publish"),
        std::ffi::OsStr::to_os_string,
    );
    name.push(".tmp");
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}
