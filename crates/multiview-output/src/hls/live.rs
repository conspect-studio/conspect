//! HLS-0/HLS-1 rolling **live** playlist driver (ADR-0032).
//!
//! A live multiview run never finalizes: it produces segments forever. The batch
//! [`SegmentSink`](crate::sink::SegmentSink) model (accumulate every segment in a
//! `Vec`, render the playlist once at the end) therefore never writes the
//! `.m3u8` and grows the segment set without bound. [`LivePlaylist`] is the
//! rolling driver that fixes both: on **each** closed segment it pushes the
//! segment into a windowed [`MediaPlaylist`], re-renders, and publishes the
//! manifest to disk **atomically** (same-dir `.tmp` → fsync → `rename(2)`), then
//! retains evicted `.ts` files for one further bounded client-grace window.
//!
//! This module is **pure Rust** — it performs only filesystem I/O and drives the
//! pure-text [`MediaPlaylist`] generator; it pulls no native dependency and is
//! compiled in every build (the encoder-fed segment muxing that feeds it lives
//! behind the `ffmpeg` feature). It upholds invariants #1/#10 by construction:
//! it runs entirely on the off-hot-path egress thread and never blocks the output
//! clock or back-pressures the engine.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{MediaPlaylist, Segment, SegmentType};
use crate::epoch::SharedEpoch;
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
    /// The listed window; one additional window remains on disk as client grace.
    window: usize,
    /// The windowed media playlist; `set_window(window)` is applied at
    /// construction so [`MediaPlaylist::push_segment`] evicts + advances the media
    /// (and discontinuity) sequence per RFC 8216bis §6.2.2.
    playlist: MediaPlaylist,
    /// Published segments currently listed in the manifest, oldest first.
    seg_paths: VecDeque<PathBuf>,
    /// Recently retired segments remain available through one complete playlist
    /// window (at least three segment closes). This queue is bounded separately.
    retired_paths: VecDeque<PathBuf>,
    /// The shared outbound presentation epoch (ADR-M010, DEV-C1). When set
    /// **and** the cell carries an epoch, each closed segment is stamped with
    /// `EXT-X-PROGRAM-DATE-TIME: epoch.wall_at(segment first PTS)` — the same
    /// map the control WS publishes and RTCP SR stamps. `None` (or an empty
    /// cell) ⇒ no PDT tags (behaviour identical to before this field).
    epoch: Option<SharedEpoch>,
    /// The epoch generation of the last segment close that saw an epoch
    /// (`None` before the first). A **changed** generation means the sampler
    /// stepped the map (a Class-2-like re-anchor, wall-clock-sync §3): the
    /// first segment closed under the new map is marked
    /// `EXT-X-DISCONTINUITY`. Hold/slew updates keep the generation, so they
    /// mark nothing.
    epoch_generation: Option<u64>,
    /// A new encoder run starts an independent timestamp sequence.
    initial_discontinuity: bool,
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
            retired_paths: VecDeque::new(),
            epoch: None,
            epoch_generation: None,
            initial_discontinuity: false,
        }
    }

    /// Set the next media sequence when a restarted run uses non-reused segment names.
    pub fn set_media_sequence(&mut self, sequence: u64) {
        self.playlist.set_media_sequence(sequence);
    }

    /// Mark the first segment after a restarted encoder as discontinuous.
    pub fn set_initial_discontinuity(&mut self) {
        self.initial_discontinuity = true;
    }

    /// The on-disk path the manifest is published to.
    #[must_use]
    pub fn playlist_path(&self) -> &Path {
        &self.playlist_path
    }

    /// Attach the shared outbound presentation epoch (ADR-M010): every
    /// subsequently-closed segment is stamped with an
    /// `EXT-X-PROGRAM-DATE-TIME` derived from `epoch.wall_at(start_pts)`.
    /// Until the cell carries an epoch (the ~1 Hz sampler has not anchored
    /// yet), segments are published without a PDT — never a fabricated wall
    /// time.
    pub fn set_epoch_source(&mut self, epoch: SharedEpoch) {
        self.epoch = Some(epoch);
    }

    /// Record one **closed** GOP-aligned segment: append it to the windowed
    /// playlist (`uri` referenced relative to the manifest, `duration` its
    /// `EXTINF` seconds, `start_pts_ns` the output PTS of its first sample on
    /// the tick-derived internal timeline), recompute `EXT-X-TARGETDURATION`,
    /// and atomically re-publish the manifest to disk. The segment's on-disk
    /// `path` is tracked so that, once it ages beyond the client-grace window,
    /// its `.ts` file is pruned.
    ///
    /// When an epoch source is attached ([`set_epoch_source`]) and carries an
    /// epoch, the segment is stamped `EXT-X-PROGRAM-DATE-TIME:
    /// epoch.wall_at(start_pts_ns)` — exact integer affine math, the same
    /// anchor every other ADR-M010 surface uses. When the epoch **generation**
    /// changed since the previous close (the sampler published a *stepped*
    /// re-anchor via [`SharedEpoch::set_stepped`]), this first segment under
    /// the new map is additionally marked `EXT-X-DISCONTINUITY`
    /// (wall-clock-sync §3: a step follows the same Class-2 discontinuity
    /// rules as a `D` change); hold/slew updates keep the generation and mark
    /// nothing.
    ///
    /// The published manifest carries **no** `#EXT-X-ENDLIST` (the run is live);
    /// [`finalize`](Self::finalize) adds it once at end-of-run.
    ///
    /// [`set_epoch_source`]: Self::set_epoch_source
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the manifest could not be atomically written
    /// (the temp write, fsync, or rename failed). Pruning an aged-out segment is
    /// best-effort and never surfaces an error; a missing file is ignored.
    pub fn push_closed_segment(
        &mut self,
        uri: impl Into<String>,
        path: PathBuf,
        duration: f64,
        start_pts_ns: i64,
    ) -> Result<()> {
        let mut segment = Segment::new(uri, duration);
        segment.discontinuity = self.initial_discontinuity;
        let mut next_generation = self.epoch_generation;
        // PDT from the shared epoch (ADR-M010): segment first-sample wall time
        // = wall_at(first PTS). An unset source or a still-empty cell stamps
        // nothing — never a fabricated wall time.
        if let Some((epoch, generation)) = self
            .epoch
            .as_ref()
            .and_then(SharedEpoch::get_with_generation)
        {
            segment = segment.with_program_date_time_ns(epoch.wall_at(start_pts_ns));
            // The step seam (wall-clock-sync §3): a generation bump since the
            // last close means the sampler STEPPED the map — mark exactly this
            // first segment closed under the new map discontinuous. The first
            // epoch ever seen (anchor) is not a step.
            if self
                .epoch_generation
                .is_some_and(|previous| previous != generation)
            {
                segment.discontinuity = true;
            }
            next_generation = Some(generation);
        }
        // Prepare a candidate, but do not change committed state until the
        // atomic rename succeeds. A failed write must leave every old URI valid.
        let mut next_playlist = self.playlist.clone();
        // Append to the windowed playlist; `push_segment` auto-evicts the oldest
        // beyond the window and advances EXT-X-MEDIA-SEQUENCE / -DISCONTINUITY-
        // SEQUENCE (RFC 8216bis §6.2.2) — we never touch those counters by hand.
        next_playlist.push_segment(segment);
        // TARGETDURATION must be >= every EXTINF still listed, as an integer.
        next_playlist.recompute_target_duration();
        atomic_write(&self.playlist_path, next_playlist.render().as_bytes())?;
        self.playlist = next_playlist;
        self.initial_discontinuity = false;
        self.epoch_generation = next_generation;
        self.seg_paths.push_back(path);
        while self.seg_paths.len() > self.window {
            if let Some(evicted) = self.seg_paths.pop_front() {
                self.retired_paths.push_back(evicted);
            }
        }
        // An old client can still hold a recently superseded manifest. Retain
        // one further full window before unlinking, without unbounded growth.
        let references = self
            .playlist_path
            .parent()
            .and_then(media_segment_references);
        while self.retired_paths.len() > self.window.max(3) {
            if let (Some(expired), Some(references)) = (self.retired_paths.pop_front(), &references)
            {
                // An old generation or a stalled sibling can still list this TS.
                if !references.contains(&expired.file_name().unwrap_or_default().to_os_string()) {
                    prune_segment(&expired);
                }
            }
        }
        Ok(())
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

/// Parse every media URI line before checking its extension. A non-TS URI in
/// a TS media playlist is uncertain, not proof that no TS is referenced.
/// Unknown URI forms fail closed so neither cleanup path can unlink media.
pub(crate) fn media_segment_uris(text: &str) -> Option<Vec<String>> {
    let mut uris = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let (path, suffix) = line.split_once(['?', '#']).unwrap_or((line, ""));
        // Query and fragment may include escaped values, but malformed percent
        // escapes or control bytes leave the reference uncertain.
        if suffix.bytes().any(|byte| byte.is_ascii_control()) || !valid_escapes(suffix) {
            return None;
        }
        let mut decoded = Vec::with_capacity(path.len());
        let mut bytes = path.bytes();
        while let Some(byte) = bytes.next() {
            if byte == b'%' {
                let high = bytes.next().and_then(hex_digit)?;
                let low = bytes.next().and_then(hex_digit)?;
                decoded.push((high << 4) | low);
            } else {
                decoded.push(byte);
            }
        }
        let name = String::from_utf8(decoded).ok()?;
        // Reject separators, URI schemes, drive syntax and any remaining '%':
        // a second decode must never turn a basename into traversal.
        if name.is_empty()
            || matches!(name.as_str(), "." | "..")
            || name.contains(['/', '\\', ':', '%', '?', '#'])
            || name.bytes().any(|byte| byte.is_ascii_control())
        {
            return None;
        }
        match Path::new(&name).extension() {
            Some(ext) if ext.eq_ignore_ascii_case("ts") => uris.push(name),
            // Master playlists list media playlists, not media segments.
            Some(ext) if ext.eq_ignore_ascii_case("m3u8") => {}
            _ => return None,
        }
    }
    Some(uris)
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|digit| u8::try_from(digit).ok())
}

fn valid_escapes(input: &str) -> bool {
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%'
            && (bytes.next().and_then(hex_digit).is_none()
                || bytes.next().and_then(hex_digit).is_none())
        {
            return false;
        }
    }
    true
}

/// Exact same-directory TS basenames named by any still-present media manifest.
/// Fail closed on unreadable or uncertain manifests: callers must not unlink.
pub(crate) fn media_segment_references(dir: &Path) -> Option<HashSet<OsString>> {
    let mut references = HashSet::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if !entry
            .path()
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("m3u8"))
        {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).ok()?;
        references.extend(media_segment_uris(&text)?.into_iter().map(OsString::from));
    }
    Some(references)
}

/// Best-effort unlink of an evicted segment file. The result is deliberately
/// discarded: a `NotFound` means the file is already gone, and any other unlink
/// error is swallowed rather than failing the live run. A complete additional
/// playlist window has elapsed before this unlink. The non-propagation keeps a
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
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
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
