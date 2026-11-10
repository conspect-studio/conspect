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
use std::sync::{Arc, Condvar, Mutex};

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
    /// The off-packet-path publisher (#220), when enabled. `None` keeps the
    /// original synchronous publish (write + fsync + rename inline).
    background: Option<BackgroundPublisher>,
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
            background: None,
        }
    }

    /// Publish manifests on a dedicated `hls-publish` thread instead of inline
    /// (#220, `docs/architecture/hls-manifest-publication.md`): each
    /// [`push_closed_segment`](Self::push_closed_segment) renders in memory and
    /// hands the newest manifest to the publisher through a latest-wins slot, so
    /// a slow `fsync` can never block the caller (the sink's packet path).
    /// A publish error is reported by a later call or by
    /// [`finalize`](Self::finalize). Idempotent.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the publisher thread cannot be spawned.
    pub fn enable_background_publish(&mut self) -> Result<()> {
        self.enable_background_publish_with(Arc::new(|path: &Path, bytes: &[u8]| {
            atomic_write(path, bytes)
        }))
    }

    /// [`enable_background_publish`](Self::enable_background_publish) with an
    /// injectable manifest writer — the seam the slow-disk chaos tests use.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the publisher thread cannot be spawned.
    #[doc(hidden)]
    pub fn enable_background_publish_with(&mut self, writer: ManifestWriter) -> Result<()> {
        if self.background.is_none() {
            self.background = Some(BackgroundPublisher::spawn(
                self.playlist_path.clone(),
                writer,
            )?);
        }
        Ok(())
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
        if self.background.is_some() {
            // Off the packet path: surface an earlier publish failure, then hand
            // the newest manifest + the segments it retired to the publisher.
            if let Some(background) = self.background.as_ref() {
                background.take_error()?;
            }
            let text = next_playlist.render();
            self.commit(next_playlist, next_generation, path);
            let mut retired = Vec::new();
            while self.retired_paths.len() > self.window.max(3) {
                if let Some(expired) = self.retired_paths.pop_front() {
                    retired.push(expired);
                }
            }
            if let Some(background) = self.background.as_ref() {
                background.submit(text, retired, false);
            }
            return Ok(());
        }
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
        if let Some(background) = self.background.take() {
            background.submit(self.playlist.render(), Vec::new(), true);
            return background.join();
        }
        self.publish()
    }

    /// Commit an accepted segment close to the in-memory state (background mode):
    /// the playlist, epoch generation, and the listed/retired segment queues.
    fn commit(&mut self, playlist: MediaPlaylist, generation: Option<u64>, path: PathBuf) {
        self.playlist = playlist;
        self.initial_discontinuity = false;
        self.epoch_generation = generation;
        self.seg_paths.push_back(path);
        while self.seg_paths.len() > self.window {
            if let Some(evicted) = self.seg_paths.pop_front() {
                self.retired_paths.push_back(evicted);
            }
        }
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

/// A manifest writer: `(path, bytes)` → durable atomic publish. Production uses
/// [`atomic_write`]; tests inject slow or failing writers.
pub type ManifestWriter = Arc<dyn Fn(&Path, &[u8]) -> Result<()> + Send + Sync>;

/// The most retired segments a run of failed publishes may carry before the
/// oldest are dropped from the list (left on disk, never unlinked) — keeps a
/// persistently failing disk from growing memory without bound.
const MAX_CARRIED_RETIRED: usize = 1024;

/// One pending publish: the newest manifest text, every segment retired since the
/// last successful publish, and whether this is the final (`ENDLIST`) manifest.
#[derive(Debug, Default)]
struct Pending {
    text: Option<String>,
    retired: Vec<PathBuf>,
    last: bool,
}

#[derive(Debug, Default)]
struct Slot {
    pending: Pending,
    error: Option<String>,
    finished: bool,
}

/// Latest-wins hand-off between the sink (producer) and the `hls-publish` thread.
struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
}

/// The off-packet-path manifest publisher (#220).
struct BackgroundPublisher {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for BackgroundPublisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackgroundPublisher")
            .finish_non_exhaustive()
    }
}

impl BackgroundPublisher {
    fn spawn(playlist_path: PathBuf, writer: ManifestWriter) -> Result<Self> {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("hls-publish".to_owned())
            .spawn(move || publisher_main(&worker, &playlist_path, &writer))
            .map_err(|e| Error::Output(format!("spawning hls-publish thread: {e}")))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Replace the pending manifest (latest wins) and append newly retired
    /// segments. Never blocks on I/O: the mutex is only held for this swap.
    fn submit(&self, text: String, retired: Vec<PathBuf>, last: bool) {
        let mut slot = lock(&self.shared.slot);
        slot.pending.text = Some(text);
        slot.pending.retired.extend(retired);
        let overflow = slot
            .pending
            .retired
            .len()
            .saturating_sub(MAX_CARRIED_RETIRED);
        if overflow > 0 {
            slot.pending.retired.drain(..overflow);
        }
        slot.pending.last |= last;
        drop(slot);
        self.shared.wake.notify_one();
    }

    /// Return (and clear) the error of a failed background publish, if any.
    fn take_error(&self) -> Result<()> {
        match lock(&self.shared.slot).error.take() {
            Some(e) => Err(Error::Output(e)),
            None => Ok(()),
        }
    }

    /// Wait for the final manifest to be published and the thread to exit.
    fn join(mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| Error::Output("hls-publish thread panicked".to_owned()))?;
        }
        self.take_error()
    }
}

impl Drop for BackgroundPublisher {
    fn drop(&mut self) {
        // An abandoned (never finalized) playlist: let the thread finish the
        // pending publish and exit; do not wait on it here.
        lock(&self.shared.slot).finished = true;
        self.shared.wake.notify_one();
    }
}

/// Lock the slot, recovering from poisoning (the data is plain values; a panic
/// elsewhere must not wedge publication).
fn lock(m: &Mutex<Slot>) -> std::sync::MutexGuard<'_, Slot> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The `hls-publish` loop: take the newest pending manifest, publish it, then
/// prune the segments it retired that no on-disk manifest still lists. A failed
/// publish keeps its retired segments for the next attempt and records the error.
fn publish_one(shared: &Shared, playlist_path: &Path, writer: &ManifestWriter, job: Pending) {
    let Some(text) = job.text else { return };
    match writer(playlist_path, text.as_bytes()) {
        Ok(()) => {
            let references = playlist_path.parent().and_then(media_segment_references);
            if let Some(references) = references {
                for expired in job.retired {
                    let name = expired.file_name().unwrap_or_default().to_os_string();
                    if !references.contains(&name) {
                        prune_segment(&expired);
                    }
                }
            }
        }
        Err(e) => {
            let mut slot = lock(&shared.slot);
            // Keep the retired segments for the next successful publish (older
            // first) so nothing listed by the last good manifest is unlinked.
            let mut carried = job.retired;
            carried.append(&mut slot.pending.retired);
            slot.pending.retired = carried;
            slot.error = Some(e.to_string());
        }
    }
}

fn publisher_main(shared: &Shared, playlist_path: &Path, writer: &ManifestWriter) {
    loop {
        let (job, exit) = {
            let mut slot = lock(&shared.slot);
            while slot.pending.text.is_none() && !slot.finished {
                slot = shared
                    .wake
                    .wait(slot)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            let job = std::mem::take(&mut slot.pending);
            let exit = job.last || (slot.finished && job.text.is_none());
            (job, exit)
        };
        let last = job.last;
        publish_one(shared, playlist_path, writer, job);
        if exit || last {
            return;
        }
    }
}
