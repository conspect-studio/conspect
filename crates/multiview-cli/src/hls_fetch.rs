//! Live HLS segment fetcher: parallel byte-range downloads, served to libav
//! from memory over loopback.
//!
//! libav's HLS demuxer fetches one segment at a time over one TCP connection.
//! On a lossy, high-RTT path (mda001 → cnn-usa: ~300 ms RTT, ~5 % loss) one TCP
//! flow delivers only ~30–60 KB/s, i.e. 0.2–0.4x real time for a 780 KB / 5.6 s
//! segment. No buffer can hide a source that is delivered slower than it plays.
//! This module fetches each segment as several parallel `Range` requests (whole
//! GETs when the origin does not support ranges), keeps the next segments in
//! flight, retries, and republishes completed segments in an in-memory media
//! playlist served on `[::1]` so the unchanged libav demuxer reads them at
//! memory speed.
//!
//! Control/IO plane only: every thread here is a network or loopback-server
//! thread. Nothing touches the output clock or blocks the data plane
//! (invariants #1/#10). Memory is bounded: at most [`WINDOW`] segments are kept.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Set (to anything) to disable the fetcher: live HLS opens the origin with
/// libav directly, as before. An operator escape hatch.
pub const DISABLE_ENV: &str = "MULTIVIEW_DISABLE_HLS_FETCH";

/// Completed segments kept for the local playlist (bounded memory).
const WINDOW: usize = 8;
/// Longest accepted request or header line, terminator included.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// Most header lines accepted in one request.
const MAX_HEADERS: usize = 64;
/// Most bytes accepted in one request head.
const MAX_HEAD_BYTES: usize = 32 * 1024;
/// How long a whole request head may take once its first byte arrives.
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
/// How long a keep-alive connection may sit between requests.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Socket timeout slice, which bounds how late shutdown is noticed.
const IO_POLL: Duration = Duration::from_millis(100);
/// Most unread bytes discarded after a refusal.
const REFUSAL_DRAIN_BYTES: usize = 256 * 1024;
/// Longest time spent discarding input after a refusal.
const REFUSAL_DRAIN_DEADLINE: Duration = Duration::from_secs(1);
/// The refusal for a request head over its bounds.
const HEAD_TOO_LARGE: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
/// Segments fetched concurrently.
const IN_FLIGHT: usize = 3;
/// Parallel ranges per segment after the probe range.
const RANGE_PARTS: u64 = 6;
/// The first (probe) range: learns the size and whether ranges work.
const PROBE_BYTES: u64 = 64 * 1024;
/// Attempts per range / whole GET before the segment attempt fails.
const RANGE_ATTEMPTS: u32 = 4;
/// TCP connect timeout (origins on mda001 sometimes take 8 s to accept; fail
/// fast and retry rather than wait).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Upper bound for one HTTP call. Generous: a no-`Range` origin sends a whole
/// segment in one call at single-flow speed.
const CALL_TIMEOUT: Duration = Duration::from_secs(40);
/// Default pause after a 429/503 without a usable `Retry-After`, and the cap on
/// any pause, so a bad header cannot silence a source for long.
const THROTTLE_DEFAULT: Duration = Duration::from_secs(5);
const THROTTLE_MAX: Duration = Duration::from_secs(60);
/// Clean segments before parallelism steps back up one level.
const CLEAN_TO_RECOVER: u64 = 10;
/// Pause between probes of a listed segment that is not on the origin yet.
const RETRY_PAUSE_MAX_MS: u64 = 1_000;
/// The largest segment accepted.
const MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// With nothing published for this long the origin counts as gone: the local
/// playlist answers 404, libav's read fails and `ingest_loop` reconnects. Without
/// this libav would poll a healthy loopback playlist forever.
const DEAD_ORIGIN: Duration = Duration::from_secs(30);
/// How long [`HlsFetcher::start`] waits for the first segment.
const FIRST_SEGMENT_DEADLINE: Duration = Duration::from_secs(40);

/// One media segment listed by the origin playlist.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OriginSegment {
    /// Media sequence number.
    pub seq: u64,
    /// `#EXTINF` duration in seconds.
    pub duration: f64,
    /// Preceded by `#EXT-X-DISCONTINUITY`.
    pub discontinuity: bool,
    /// The URI as written (relative or absolute).
    pub uri: String,
}

/// The parts of a live media playlist the fetcher needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OriginPlaylist {
    /// `#EXT-X-TARGETDURATION` in seconds.
    pub target_duration: f64,
    /// The listed segments, in order.
    pub segments: Vec<OriginSegment>,
    /// Bounded original manifest evidence; never inferred splice boundaries.
    pub cue_metadata: multiview_input::hls::PlaylistCueMetadata,
}

/// Why a playlist cannot be fetched this way (the caller lets libav open the
/// origin directly instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unsupported {
    /// Not a playlist.
    NotAPlaylist,
    /// A master playlist (variant pinning happens before this).
    Master,
    /// `#EXT-X-ENDLIST`: not live.
    Vod,
    /// Encryption, init sections or byte-range segments.
    Feature(&'static str),
}

/// Parse a live media playlist.
pub(crate) fn parse_media_playlist(text: &str) -> Result<OriginPlaylist, Unsupported> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(Unsupported::NotAPlaylist);
    }
    let mut target_duration = 0.0_f64;
    let mut seq = 0_u64;
    let mut duration: Option<f64> = None;
    let mut pending_segment = false;
    let mut discontinuity = false;
    let mut segments = Vec::new();
    for line in lines {
        if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = v.trim().parse().unwrap_or(0.0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            if pending_segment {
                return Err(Unsupported::Feature("incomplete media snapshot"));
            }
            pending_segment = true;
            duration = v.split(',').next().and_then(|d| d.trim().parse().ok());
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuity = true;
        } else if line == "#EXT-X-ENDLIST" {
            return Err(Unsupported::Vod);
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            return Err(Unsupported::Master);
        } else if line.starts_with("#EXT-X-SKIP:") {
            // Delta updates cannot prove membership absence without reconstruction.
            return Err(Unsupported::Feature("EXT-X-SKIP"));
        } else if line.starts_with("#EXT-X-MAP") {
            return Err(Unsupported::Feature("EXT-X-MAP"));
        } else if line.starts_with("#EXT-X-BYTERANGE") {
            return Err(Unsupported::Feature("EXT-X-BYTERANGE"));
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            if !attrs.contains("METHOD=NONE") {
                return Err(Unsupported::Feature("EXT-X-KEY"));
            }
        } else if !line.starts_with('#') {
            pending_segment = false;
            segments.push(OriginSegment {
                seq,
                duration: duration.take().unwrap_or(target_duration),
                discontinuity: std::mem::take(&mut discontinuity),
                uri: line.to_owned(),
            });
            seq = seq.saturating_add(1);
        }
    }
    if segments.is_empty() || pending_segment {
        return Err(Unsupported::Feature("incomplete media snapshot"));
    }
    if target_duration <= 0.0 {
        target_duration = segments.iter().map(|s| s.duration).fold(0.0, f64::max);
    }
    Ok(OriginPlaylist {
        target_duration: target_duration.max(1.0),
        segments,
        cue_metadata: multiview_input::hls::MediaPlaylist::parse_cue_metadata(text).unwrap_or_else(
            |_| multiview_input::hls::PlaylistCueMetadata {
                rejected: 1,
                ..Default::default()
            },
        ),
    })
}

/// Split `[start, total)` into at most `parts` contiguous inclusive ranges.
pub(crate) fn split_ranges(start: u64, total: u64, parts: u64) -> Vec<(u64, u64)> {
    if start >= total || parts == 0 {
        return Vec::new();
    }
    let len = total - start;
    let parts = parts.min(len);
    let base = len / parts;
    let extra = len % parts;
    let mut out = Vec::with_capacity(usize::try_from(parts).unwrap_or(0));
    let mut at = start;
    for i in 0..parts {
        let n = base + u64::from(i < extra);
        out.push((at, at + n - 1));
        at += n;
    }
    out
}

/// Parse the total from `Content-Range: bytes a-b/total`.
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit('/').next()?.trim().parse().ok()
}

/// Per-source politeness toward the origin. A 429 or 503 pauses every request
/// for this source (honouring `Retry-After`) and halves its parallelism; clean
/// segments restore it slowly. Kept across reconnects, so a reconnect does not
/// reset a rate limit.
#[derive(Debug, Default)]
pub struct Throttle {
    /// Halvings of [`RANGE_PARTS`] in force: 0 → 6 parts, 1 → 3, 2 → one whole GET.
    level: AtomicU64,
    clean: AtomicU64,
    until: Mutex<Option<Instant>>,
    /// 429/503 responses seen.
    pub limited: AtomicU64,
}

impl Throttle {
    /// Parallel ranges per segment right now.
    fn parts(&self) -> u64 {
        match self.level.load(Ordering::Relaxed) {
            0 => RANGE_PARTS,
            1 => RANGE_PARTS / 2,
            _ => 1,
        }
    }

    /// Segments in flight right now.
    fn in_flight(&self) -> usize {
        match self.level.load(Ordering::Relaxed) {
            0 => IN_FLIGHT,
            1 => 2,
            _ => 1,
        }
    }

    /// The origin told us to slow down.
    fn penalize(&self, retry_after: Option<Duration>) {
        self.limited.fetch_add(1, Ordering::Relaxed);
        self.clean.store(0, Ordering::Relaxed);
        let pause = retry_after.unwrap_or(THROTTLE_DEFAULT).min(THROTTLE_MAX);
        let mut until = self.until.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        // Concurrent 429s from one burst are one event: halve only once per pause.
        if until.is_none_or(|u| u <= now) {
            let level = self.level.load(Ordering::Relaxed);
            self.level.store((level + 1).min(2), Ordering::Relaxed);
        }
        let end = now + pause;
        *until = Some(until.map_or(end, |u| u.max(end)));
    }

    /// A segment finished cleanly.
    fn segment_ok(&self) {
        if self.clean.fetch_add(1, Ordering::Relaxed) + 1 >= CLEAN_TO_RECOVER {
            self.clean.store(0, Ordering::Relaxed);
            let level = self.level.load(Ordering::Relaxed);
            self.level.store(level.saturating_sub(1), Ordering::Relaxed);
        }
    }

    /// How long requests must still wait.
    fn remaining(&self) -> Duration {
        self.until
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .map_or(Duration::ZERO, |u| {
                u.saturating_duration_since(Instant::now())
            })
    }
}

/// Parse a `Retry-After` in seconds (the HTTP-date form falls back to the default).
fn retry_after(value: &str) -> Option<Duration> {
    value.trim().parse().ok().map(Duration::from_secs)
}

/// A failed attempt, and whether trying again can help.
#[derive(Debug)]
struct Failure {
    message: String,
    fatal: bool,
}

impl Failure {
    fn retry(message: String) -> Self {
        Self {
            message,
            fatal: false,
        }
    }

    /// From a status. Other 4xx (403, 410, ...) are final: repeating them only
    /// looks like abuse. 404 is retried: a new segment can lag its playlist.
    fn status(what: &str, status: u16) -> Self {
        Self {
            message: format!("{what}: http {status}"),
            fatal: (400..500).contains(&status) && !matches!(status, 404 | 408 | 429),
        }
    }
}

/// Live fetch statistics for one source (lock-free; read by reporting).
#[derive(Debug, Default)]
pub struct FetchStats {
    /// Segments fetched and published.
    pub segments_fetched: AtomicU64,
    /// Segments given up on (expired or failing past their deadline).
    pub segments_skipped: AtomicU64,
    /// Bytes downloaded.
    pub bytes: AtomicU64,
    /// Last segment's fetch rate: media seconds per wall second, x1000.
    pub last_rate_milli: AtomicU64,
    /// Smoothed fetch rate (EWMA, alpha 1/4), x1000.
    pub avg_rate_milli: AtomicU64,
    /// 1 when the origin served ranges for the last segment, else 0.
    pub ranges_supported: AtomicU64,
    /// Set when a fetcher gave up on the origin: the next connect lets libav
    /// open it directly (then the flag clears), so a fetcher-specific failure
    /// cannot keep a source down.
    pub bypass_next: AtomicBool,
    /// Politeness state toward this origin.
    pub throttle: Throttle,
}

impl FetchStats {
    fn record(&self, bytes: usize, media: f64, wall: Duration, ranged: bool) {
        self.segments_fetched.fetch_add(1, Ordering::Relaxed);
        self.bytes
            .fetch_add(u64::try_from(bytes).unwrap_or(0), Ordering::Relaxed);
        self.ranges_supported
            .store(u64::from(ranged), Ordering::Relaxed);
        let rate = if wall.is_zero() {
            0
        } else {
            // Float only for a human-facing statistic, never for timing.
            let micros = wall.as_micros().max(1);
            let media_micros = Duration::from_secs_f64(media.clamp(0.0, 1e6)).as_micros();
            u64::try_from(media_micros.saturating_mul(1000) / micros).unwrap_or(u64::MAX)
        };
        self.last_rate_milli.store(rate, Ordering::Relaxed);
        let prev = self.avg_rate_milli.load(Ordering::Relaxed);
        let avg = if prev == 0 {
            rate
        } else {
            (prev * 3 + rate) / 4
        };
        self.avg_rate_milli.store(avg, Ordering::Relaxed);
    }
}

/// The process-wide [`FetchStats`] for source `id` (one `Arc` per id).
pub fn stats(id: &str) -> Arc<FetchStats> {
    static STATS: OnceLock<Mutex<HashMap<String, Arc<FetchStats>>>> = OnceLock::new();
    let map = STATS.get_or_init(Default::default);
    let mut map = map.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(map.entry(id.to_owned()).or_default())
}

// Control-plane/coordinator only: never acquired by the output path.
type ObservationGate = Mutex<Option<crate::hls_event_observer::HlsEventObserver>>;

fn retire_observer(gate: Option<Arc<ObservationGate>>) {
    if let Some(gate) = gate {
        if let Some(mut observer) = gate.lock().unwrap_or_else(PoisonError::into_inner).take() {
            observer.retire();
        }
    }
}

#[derive(Default)]
struct FetchRegistration {
    generation: u64,
    url: Option<String>,
    observation_active: Option<std::sync::Weak<ObservationGate>>,
}

fn urls() -> &'static Mutex<HashMap<String, FetchRegistration>> {
    static URLS: OnceLock<Mutex<HashMap<String, FetchRegistration>>> = OnceLock::new();
    URLS.get_or_init(Default::default)
}

/// Invalidate the old fetcher before the replacement audio or video starts.
/// URL publication uses the registry lock. Observation invalidation completes
/// before this returns: it waits for the old off-clock batch, then retires its
/// sink outside URL/window locks. It is not an output-path operation.
pub(crate) fn begin_generation(id: &str) -> u64 {
    let mut map = urls().lock().unwrap_or_else(PoisonError::into_inner);
    let entry = map.entry(id.to_owned()).or_default();
    let old = entry.observation_active.take().and_then(|w| w.upgrade());
    entry.generation = entry.generation.saturating_add(1);
    entry.url = None;
    let generation = entry.generation;
    drop(map);
    // Completion fence: wait for an in-flight batch, then invalidate its sink.
    // Never invoke a sink callback while holding the URL/window locks.
    retire_observer(old);
    generation
}

/// Install the local fetch only while its lifecycle still owns this id.
fn publish_url(id: &str, generation: u64, local_url: &str) -> Result<(), String> {
    let mut map = urls().lock().unwrap_or_else(PoisonError::into_inner);
    let entry = map.entry(id.to_owned()).or_default();
    if entry.generation != generation {
        return Err("source replaced before fetch publication".to_owned());
    }
    entry.url = Some(local_url.to_owned());
    Ok(())
}

/// Return only the fetched copy belonging to this exact source lifecycle.
#[must_use]
pub fn shared_url(id: &str, generation: u64) -> Option<String> {
    urls()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(id)
        .filter(|entry| entry.generation == generation)
        .and_then(|entry| entry.url.clone())
}

/// A completed segment in the local window.
struct LocalSegment {
    local_seq: u64,
    duration: f64,
    discontinuity: bool,
    bytes: Arc<Vec<u8>>,
}

/// State shared by the coordinator and the loopback server.
struct Shared {
    window: Mutex<Window>,
    ready: Condvar,
    stop: AtomicBool,
}

struct Window {
    target_duration: f64,
    segments: VecDeque<LocalSegment>,
    next_local: u64,
    /// The coordinator gave up (unsupported playlist after start, etc.).
    failed: Option<String>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Window> {
        self.window.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn publish(&self, duration: f64, discontinuity: bool, bytes: Vec<u8>) {
        let mut w = self.lock();
        let local_seq = w.next_local;
        w.next_local += 1;
        w.segments.push_back(LocalSegment {
            local_seq,
            duration,
            discontinuity,
            bytes: Arc::new(bytes),
        });
        while w.segments.len() > WINDOW {
            w.segments.pop_front();
        }
        drop(w);
        self.ready.notify_all();
    }

    /// The local playlist, or `None` once the origin is given up on.
    fn playlist(&self) -> Option<String> {
        let w = self.lock();
        if w.failed.is_some() {
            return None;
        }
        let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:3\n");
        let target = w.target_duration.ceil().max(1.0);
        let _ = writeln!(out, "#EXT-X-TARGETDURATION:{target}");
        let first = w.segments.front().map_or(w.next_local, |s| s.local_seq);
        let _ = writeln!(out, "#EXT-X-MEDIA-SEQUENCE:{first}");
        for s in &w.segments {
            if s.discontinuity {
                out.push_str("#EXT-X-DISCONTINUITY\n");
            }
            let _ = write!(out, "#EXTINF:{:.6},\n/s/{}.ts\n", s.duration, s.local_seq);
        }
        Some(out)
    }

    /// Give up on the origin (see [`DEAD_ORIGIN`]).
    fn fail(&self, reason: String) {
        self.lock().failed = Some(reason);
        self.ready.notify_all();
    }

    fn segment(&self, local_seq: u64) -> Option<Arc<Vec<u8>>> {
        self.lock()
            .segments
            .iter()
            .find(|s| s.local_seq == local_seq)
            .map(|s| Arc::clone(&s.bytes))
    }
}

/// A running fetcher for one live media playlist. Dropping it stops the
/// coordinator and the loopback server.
pub(crate) struct HlsFetcher {
    shared: Arc<Shared>,
    source_id: String,
    local_url: String,
    generation: u64,
    observation_gate: Arc<ObservationGate>,
}

impl Drop for HlsFetcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        // Only our own entry: a replacement may already have registered.
        let mut map = urls().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = map.get_mut(&self.source_id) {
            if entry.generation == self.generation && entry.url.as_ref() == Some(&self.local_url) {
                entry.url = None;
            }
        }
        drop(map);
        // Owned by the ingest supervisor, never the output clock. Complete the
        // in-flight evidence batch and retire its sink before teardown returns.
        retire_observer(Some(Arc::clone(&self.observation_gate)));
    }
}

impl HlsFetcher {
    /// Start fetching `playlist_url` (a live MEDIA playlist) and wait until its
    /// first segment is in memory. Returns `Err` when the playlist cannot be
    /// handled this way (the caller opens the origin with libav instead), when
    /// nothing arrives in time, or when `stop` is raised.
    pub(crate) fn start(
        source_id: &str,
        playlist_url: &str,
        stop: &AtomicBool,
        generation: u64,
    ) -> Result<Self, String> {
        Self::start_with_observer(source_id, playlist_url, stop, generation, None)
    }

    /// Inject a bounded observer owned solely by this fetch coordinator.
    /// The caller must replace service registration before beginning a new fetch generation.
    pub(crate) fn start_with_observer(
        source_id: &str,
        playlist_url: &str,
        stop: &AtomicBool,
        generation: u64,
        observer: Option<crate::hls_event_observer::HlsEventObserver>,
    ) -> Result<Self, String> {
        let has_observer = observer.is_some();
        let observation_active = Arc::new(Mutex::new(observer));
        if has_observer {
            let mut map = urls().lock().unwrap_or_else(PoisonError::into_inner);
            let entry = map
                .get_mut(source_id)
                .filter(|e| e.generation == generation)
                .ok_or("source replaced before observer attachment")?;
            let old = entry
                .observation_active
                .replace(Arc::downgrade(&observation_active))
                .and_then(|w| w.upgrade());
            drop(map);
            retire_observer(old);
        }
        let st = stats(source_id);
        if st.bypass_next.swap(false, Ordering::AcqRel) {
            return Err("bypassed after the fetcher gave up on this origin".to_owned());
        }
        let base = url::Url::parse(playlist_url).map_err(|e| format!("playlist url: {e}"))?;
        let agent = agent();
        // Probe the playlist once synchronously so an unsupported one falls back
        // before any thread starts.
        let (first, final_url) = fetch_playlist(&agent, base.as_str(), &st.throttle)?;
        // Segment URIs resolve against the playlist's final URL (RFC 8216 §4.1),
        // which differs from the request URL after a redirect.
        let segment_base = url::Url::parse(&final_url).unwrap_or_else(|_| base.clone());
        let shared = Arc::new(Shared {
            window: Mutex::new(Window {
                target_duration: first.target_duration,
                segments: VecDeque::new(),
                next_local: 0,
                failed: None,
            }),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let listener = TcpListener::bind("[::1]:0").map_err(|e| format!("loopback bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let local_url = format!("http://[::1]:{port}/live.m3u8");
        spawn_server(source_id, listener, Arc::clone(&shared))?;
        let coord_shared = Arc::clone(&shared);
        let id = source_id.to_owned();
        let observation_gate = Arc::clone(&observation_active);
        std::thread::Builder::new()
            .name(format!("multiview-hls-{source_id}"))
            .spawn(move || {
                let origin = Origin {
                    id: &id,
                    agent: &agent,
                    playlist: &base,
                    segments: &segment_base,
                };
                coordinate(&origin, first, &coord_shared, &st, &observation_active);
            })
            .map_err(|e| format!("spawn hls coordinator: {e}"))?;
        let fetcher = Self {
            shared,
            source_id: source_id.to_owned(),
            local_url,
            generation,
            observation_gate,
        };
        // Wait for the first segment (stop-responsive).
        let deadline = Instant::now() + FIRST_SEGMENT_DEADLINE;
        let mut w = fetcher.shared.lock();
        loop {
            if !w.segments.is_empty() {
                break;
            }
            if let Some(err) = w.failed.clone() {
                return Err(err);
            }
            let now = Instant::now();
            if stop.load(Ordering::Acquire) || now >= deadline {
                return Err("no live segment arrived".to_owned());
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            w = fetcher
                .shared
                .ready
                .wait_timeout(w, wait)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(w);
        publish_url(source_id, generation, &fetcher.local_url)?;
        Ok(fetcher)
    }

    /// The loopback playlist URL libav should open.
    pub(crate) fn local_url(&self) -> &str {
        &self.local_url
    }
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(CALL_TIMEOUT))
        .max_idle_connections_per_host(RANGE_PARTS_USIZE * IN_FLIGHT + 2)
        .build();
    ureq::Agent::new_with_config(config)
}

const RANGE_PARTS_USIZE: usize = 6;

/// Fetch and parse the playlist; returns it with its final URL (after redirects).
fn fetch_playlist(
    agent: &ureq::Agent,
    url: &str,
    throttle: &Throttle,
) -> Result<(OriginPlaylist, String), String> {
    use ureq::ResponseExt as _;
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        // Never queue behind a rate limit here: the caller reloads later.
        if !throttle.remaining().is_zero() {
            return Err("rate limited".to_owned());
        }
        match agent.get(url).call() {
            Ok(resp) if resp.status().as_u16() == 200 => {
                let final_url = resp.get_uri().to_string();
                let body = resp
                    .into_body()
                    .with_config()
                    .limit(4 * 1024 * 1024)
                    .read_to_string()
                    .map_err(|e| e.to_string())?;
                let parsed = parse_media_playlist(&body)
                    .map_err(|u| format!("unsupported playlist: {u:?}"))?;
                return Ok((parsed, final_url));
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                if matches!(status, 429 | 503) {
                    throttle.penalize(header_retry_after(&resp));
                }
                let failure = Failure::status("playlist", status);
                if failure.fatal || status == 429 {
                    return Err(failure.message);
                }
                last = failure.message;
            }
            Err(e) => last = format!("playlist: {e}"),
        }
    }
    Err(last)
}

fn header_retry_after(resp: &ureq::http::Response<ureq::Body>) -> Option<Duration> {
    resp.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(retry_after)
}

/// One HTTP GET, optionally ranged, after any rate-limit pause. Returns
/// `(status, content-range total, body)`.
fn get(
    agent: &ureq::Agent,
    url: &str,
    range: Option<(u64, u64)>,
    throttle: &Throttle,
) -> Result<(u16, Option<u64>, Vec<u8>), String> {
    let mut req = agent.get(url).header("Accept-Encoding", "identity");
    if let Some((a, b)) = range {
        req = req.header("Range", &format!("bytes={a}-{b}"));
    }
    let resp = req.call().map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if matches!(status, 429 | 503) {
        throttle.penalize(header_retry_after(&resp));
    }
    let total = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(content_range_total);
    let body = resp
        .into_body()
        .with_config()
        .limit(MAX_SEGMENT_BYTES)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok((status, total, body))
}

/// Retry `f` up to `attempts` times while `deadline` allows, waiting out any
/// rate-limit pause first. A fatal failure stops at once.
fn with_retries<T>(
    attempts: u32,
    deadline: Instant,
    stop: &AtomicBool,
    throttle: &Throttle,
    mut f: impl FnMut() -> Result<T, Failure>,
) -> Result<T, String> {
    let mut last = String::from("no attempt");
    for attempt in 0..attempts {
        if attempt > 0 {
            let ms = (250 * u64::from(attempt.min(4))).min(RETRY_PAUSE_MAX_MS);
            std::thread::sleep(Duration::from_millis(ms));
        }
        loop {
            let pause = throttle.remaining();
            if pause.is_zero() || stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(pause.min(Duration::from_millis(100)));
        }
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            break;
        }
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.fatal => return Err(e.message),
            Err(e) => last = e.message,
        }
    }
    Err(last)
}

/// Fetch one segment: a probe range, then the rest as parallel ranges (or the
/// whole body when the origin ignores `Range`, or when throttled down to one
/// connection). Returns `(bytes, ranged)`.
pub(crate) fn fetch_segment(
    agent: &ureq::Agent,
    url: &str,
    deadline: Instant,
    stop: &AtomicBool,
    throttle: &Throttle,
) -> Result<(Vec<u8>, bool), String> {
    if throttle.parts() == 1 {
        return with_retries(u32::MAX, deadline, stop, throttle, || {
            match get(agent, url, None, throttle).map_err(Failure::retry)? {
                (200, _, body) => Ok((body, false)),
                (s, _, _) => Err(Failure::status("segment", s)),
            }
        });
    }
    // The newest listed segment can 404 briefly: keep asking until the deadline
    // rather than skip real media.
    let (status, total, probe) = with_retries(u32::MAX, deadline, stop, throttle, || {
        let r = get(agent, url, Some((0, PROBE_BYTES - 1)), throttle).map_err(Failure::retry)?;
        match r.0 {
            200 | 206 => Ok(r),
            s => Err(Failure::status("segment", s)),
        }
    })?;
    if status == 200 {
        return Ok((probe, false));
    }
    let total = match total {
        Some(t) if t <= MAX_SEGMENT_BYTES => t,
        Some(t) => return Err(format!("segment too large: {t}")),
        None => return Err("206 without a Content-Range total".to_owned()),
    };
    let have = u64::try_from(probe.len()).unwrap_or(0);
    if have >= total {
        return Ok((probe, true));
    }
    let ranges = split_ranges(have, total, throttle.parts());
    let parts: Vec<Result<Vec<u8>, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(a, b)| {
                scope.spawn(move || {
                    with_retries(RANGE_ATTEMPTS, deadline, stop, throttle, || {
                        let (s, _, body) =
                            get(agent, url, Some((a, b)), throttle).map_err(Failure::retry)?;
                        let want = usize::try_from(b - a + 1).unwrap_or(usize::MAX);
                        if s == 206 && body.len() == want {
                            Ok(body)
                        } else if s == 206 {
                            Err(Failure::retry(format!(
                                "range {a}-{b}: {} bytes",
                                body.len()
                            )))
                        } else {
                            Err(Failure::status("range", s))
                        }
                    })
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("range thread panicked".to_owned()))
            })
            .collect()
    });
    let mut out = probe;
    out.reserve(usize::try_from(total - have).unwrap_or(0));
    for part in parts {
        out.extend_from_slice(&part?);
    }
    Ok((out, true))
}

/// The origin URLs one coordinator works against.
struct Origin<'a> {
    id: &'a str,
    agent: &'a ureq::Agent,
    /// The playlist as requested (reloads start here).
    playlist: &'a url::Url,
    /// The playlist's final URL (segment URIs resolve against it).
    segments: &'a url::Url,
}

/// A finished segment download: `(bytes, ranged, wall)` or the error.
type SegmentResult = Result<(Vec<u8>, bool, Duration), String>;

/// The coordinator's bookkeeping, separated from its I/O so it can be tested.
#[derive(Default)]
struct Schedule {
    next_seq: Option<u64>,
    /// Every segment not yet published or skipped, in order.
    order: VecDeque<OriginSegment>,
    /// The subset of `order` not yet dispatched.
    queued: VecDeque<OriginSegment>,
    done: BTreeMap<u64, SegmentResult>,
    /// The next published segment follows lost media.
    gap: bool,
}

impl Schedule {
    /// Absorb a reloaded playlist. Returns the segments lost to expiry, or
    /// `None` when the sequence ran backwards (the origin restarted).
    fn absorb(&mut self, playlist: OriginPlaylist) -> Option<u64> {
        // Start three segments from the live edge (RFC 8216 §6.3.3).
        let segments = playlist.segments;
        let start = *self.next_seq.get_or_insert_with(|| {
            let edge = segments.len().saturating_sub(3);
            segments.get(edge).map_or(0, |s| s.seq)
        });
        let window = u64::try_from(WINDOW).unwrap_or(u64::MAX);
        if segments.last().is_some_and(|s| s.seq + window < start) {
            return None;
        }
        let mut lost = 0;
        if let Some(oldest) = segments.first().map(|s| s.seq) {
            // Undispatched segments that left the playlist are gone: asking for
            // them only costs time the next segment needs. Dispatched ones stay
            // in `order`: they may well still complete.
            let expired: Vec<u64> = self
                .queued
                .iter()
                .map(|s| s.seq)
                .filter(|&seq| seq < oldest)
                .collect();
            self.queued.retain(|s| s.seq >= oldest);
            self.order.retain(|s| !expired.contains(&s.seq));
            lost += u64::try_from(expired.len()).unwrap_or(0);
            if oldest > start {
                lost += oldest - start;
                self.next_seq = Some(oldest);
            }
        }
        self.gap |= lost > 0;
        let from = self.next_seq.unwrap_or(start);
        for s in segments.into_iter().filter(|s| s.seq >= from) {
            self.next_seq = Some(s.seq + 1);
            self.order.push_back(s.clone());
            self.queued.push_back(s);
        }
        Some(lost)
    }

    /// The next segment that can be published or skipped, with its result.
    fn pop_ready(&mut self) -> Option<(OriginSegment, SegmentResult)> {
        let result = self.done.remove(&self.order.front()?.seq)?;
        Some((self.order.pop_front()?, result))
    }
}

/// The coordinator: reload the playlist, keep [`IN_FLIGHT`] segments
/// downloading, publish completed segments in order, skip ones that fail, and
/// give up after [`DEAD_ORIGIN`] without a published segment.
fn observe_playlist(
    gate: &ObservationGate,
    stop: &AtomicBool,
    metadata: &multiview_input::hls::PlaylistCueMetadata,
) {
    let mut owner = gate.lock().unwrap_or_else(PoisonError::into_inner);
    if !stop.load(Ordering::Acquire) {
        if let Some(observer) = owner.as_mut() {
            observer.observe(metadata);
        }
    }
}

#[allow(clippy::too_many_lines)]
// One sequential poll loop: reload, dispatch, collect, publish and liveness.
fn coordinate(
    origin: &Origin<'_>,
    first: OriginPlaylist,
    shared: &Arc<Shared>,
    stats: &Arc<FetchStats>,
    observation_active: &ObservationGate,
) {
    let id = origin.id;
    let mut playlist = Some(first);
    let mut sched = Schedule::default();
    let mut in_flight = 0_usize;
    let mut target = 6.0_f64;
    let mut next_reload = Instant::now();
    let mut last_progress = Instant::now();
    let (tx, rx) = mpsc::channel::<(u64, SegmentResult)>();
    while !shared.stop.load(Ordering::Acquire) {
        // (1) Playlist reload.
        if playlist.is_none() && Instant::now() >= next_reload {
            match fetch_playlist(origin.agent, origin.playlist.as_str(), &stats.throttle) {
                Ok((p, _)) => playlist = Some(p),
                Err(e) => {
                    if let Some(observer) = observation_active
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_mut()
                    {
                        observer.mark_degraded();
                    }
                    tracing::warn!(source = %id, error = %e, "hls fetch: playlist reload failed");
                    next_reload = Instant::now() + Duration::from_secs(1);
                }
            }
        }
        if let Some(p) = playlist.take() {
            // Preserve metadata before scheduling consumes the original playlist.
            // A rejected backwards snapshot must not clear marker presence.
            let metadata = p.cue_metadata.clone();
            target = p.target_duration;
            shared.lock().target_duration = target;
            let Some(lost) = sched.absorb(p) else {
                if let Some(observer) = observation_active
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                {
                    observer.mark_degraded();
                }
                shared.fail("media sequence ran backwards".to_owned());
                return;
            };
            // Outside both URL and window locks, with source service epoch fencing.
            observe_playlist(observation_active, &shared.stop, &metadata);
            if lost > 0 {
                stats.segments_skipped.fetch_add(lost, Ordering::Relaxed);
                tracing::warn!(source = %id, lost, "hls fetch: segments expired before fetch");
            }
            // Reload about every half target duration (RFC 8216 §6.3.4).
            next_reload = Instant::now() + Duration::from_secs_f64((target / 2.0).max(1.0));
        }
        // (2) Keep segments in flight.
        while in_flight < stats.throttle.in_flight() {
            let Some(seg) = sched.queued.pop_front() else {
                break;
            };
            let Ok(url) = origin.segments.join(&seg.uri) else {
                sched
                    .done
                    .insert(seg.seq, Err(format!("bad segment uri {}", seg.uri)));
                continue;
            };
            let agent = origin.agent.clone();
            let tx = tx.clone();
            let shared = Arc::clone(shared);
            let stats = Arc::clone(stats);
            // A segment older than ~3 target durations has left the playlist.
            let deadline = Instant::now() + Duration::from_secs_f64(target * 3.0);
            let seq = seg.seq;
            in_flight += 1;
            let spawned = std::thread::Builder::new()
                .name(format!("multiview-hls-seg-{id}"))
                .spawn(move || {
                    let t0 = Instant::now();
                    let r = fetch_segment(
                        &agent,
                        url.as_str(),
                        deadline,
                        &shared.stop,
                        &stats.throttle,
                    )
                    .map(|(b, ranged)| (b, ranged, t0.elapsed()));
                    let _ = tx.send((seq, r));
                });
            if spawned.is_err() {
                // Thread exhaustion: skip it rather than wedge `order` forever.
                in_flight -= 1;
                sched.done.insert(seq, Err("spawn failed".to_owned()));
                break;
            }
        }
        // (3) Collect results.
        let wait = next_reload
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(10), Duration::from_millis(200));
        if let Ok((seq, r)) = rx.recv_timeout(wait) {
            in_flight = in_flight.saturating_sub(1);
            sched.done.insert(seq, r);
            while let Ok((seq, r)) = rx.try_recv() {
                in_flight = in_flight.saturating_sub(1);
                sched.done.insert(seq, r);
            }
        }
        // (4) Publish in order.
        while let Some((seg, result)) = sched.pop_ready() {
            match result {
                Ok((bytes, ranged, wall)) => {
                    stats.record(bytes.len(), seg.duration, wall, ranged);
                    stats.throttle.segment_ok();
                    let gap = std::mem::take(&mut sched.gap);
                    shared.publish(seg.duration, seg.discontinuity || gap, bytes);
                    last_progress = Instant::now();
                }
                Err(e) => {
                    stats.segments_skipped.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(source = %id, seq = seg.seq, error = %e, "hls fetch: segment skipped");
                    sched.gap = true;
                }
            }
        }
        // (5) Liveness: libav only sees a healthy loopback, so this is the one
        // place a dead origin can surface as a reconnect.
        if last_progress.elapsed() >= DEAD_ORIGIN {
            tracing::warn!(source = %id, "hls fetch: no segment published, giving up on this connection");
            stats.bypass_next.store(true, Ordering::Release);
            shared.fail("no segment published".to_owned());
            return;
        }
    }
}

/// Serve the local playlist and segments over loopback HTTP/1.1 (keep-alive,
/// `Range` aware) until the fetcher stops.
fn spawn_server(source_id: &str, listener: TcpListener, shared: Arc<Shared>) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("loopback nonblocking: {e}"))?;
    let id = source_id.to_owned();
    std::thread::Builder::new()
        .name(format!("multiview-hls-srv-{source_id}"))
        .spawn(move || {
            while !shared.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = Arc::clone(&shared);
                        let _ = std::thread::Builder::new()
                            .name(format!("multiview-hls-conn-{id}"))
                            .spawn(move || serve_connection(stream, &shared));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
        })
        .map_err(|e| format!("spawn hls server: {e}"))?;
    Ok(())
}

/// One outcome of reading a bounded request line.
enum LineRead {
    Line(Vec<u8>),
    /// EOF, socket fault, shutdown, or an expired deadline.
    Closed,
    /// The line or the whole request head exceeded its bound.
    TooLarge,
}

/// Limits and deadlines for one request head.
struct HeadBudget {
    /// When an idle keep-alive connection is dropped.
    idle_until: Instant,
    /// Set at the request's first byte: a continuous sender cannot extend it.
    request_until: Option<Instant>,
    bytes_left: usize,
}

impl HeadBudget {
    fn new() -> Self {
        Self {
            idle_until: Instant::now() + IDLE_TIMEOUT,
            request_until: None,
            bytes_left: MAX_HEAD_BYTES,
        }
    }

    fn expired(&self, now: Instant) -> bool {
        now >= self.request_until.unwrap_or(self.idle_until)
    }
}

/// Read one line without allocating past [`MAX_LINE_BYTES`], polling so that
/// shutdown and the deadlines apply even to a client that sends nothing.
fn read_bounded_line(
    reader: &mut impl BufRead,
    stop: &AtomicBool,
    budget: &mut HeadBudget,
) -> LineRead {
    let mut line = Vec::new();
    loop {
        let now = Instant::now();
        if stop.load(Ordering::Acquire) || budget.expired(now) {
            return LineRead::Closed;
        }
        let available = match reader.fill_buf() {
            Ok([]) => return LineRead::Closed,
            Ok(available) => available,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return LineRead::Closed,
        };
        budget.request_until.get_or_insert(now + REQUEST_DEADLINE);
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if line.len() + take > MAX_LINE_BYTES || take > budget.bytes_left {
            return LineRead::TooLarge;
        }
        line.extend_from_slice(available.get(..take).unwrap_or_default());
        reader.consume(take);
        budget.bytes_left -= take;
        if newline.is_some() {
            return LineRead::Line(line);
        }
    }
}

/// Send the 431, then discard a bounded amount of unread input. Closing with
/// bytes still queued makes the kernel send RST, which destroys the refusal
/// before the client reads it. Bounded by bytes and time, so a client that
/// keeps sending cannot hold the connection open.
fn refuse_head(writer: &mut TcpStream, reader: &mut BufReader<TcpStream>) {
    if writer.write_all(HEAD_TOO_LARGE).is_err()
        || writer.shutdown(std::net::Shutdown::Write).is_err()
    {
        return;
    }
    let deadline = Instant::now() + REFUSAL_DRAIN_DEADLINE;
    let mut left = REFUSAL_DRAIN_BYTES;
    let mut scratch = [0_u8; 4096];
    while left > 0 && Instant::now() < deadline {
        match std::io::Read::read(reader, &mut scratch) {
            Ok(0) => return,
            Ok(read) => left = left.saturating_sub(read),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return,
        }
    }
}

fn serve_connection(stream: TcpStream, shared: &Shared) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(IO_POLL));
    let _ = stream.set_nodelay(true);
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut writer = write_half;
    let mut reader = BufReader::new(stream);
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let mut budget = HeadBudget::new();
        let request_line = match read_bounded_line(&mut reader, &shared.stop, &mut budget) {
            LineRead::Line(line) => String::from_utf8_lossy(&line).into_owned(),
            LineRead::Closed => return,
            LineRead::TooLarge => {
                refuse_head(&mut writer, &mut reader);
                return;
            }
        };
        let mut range: Option<(u64, Option<u64>)> = None;
        let mut close = false;
        let mut headers = 0_usize;
        loop {
            let line = match read_bounded_line(&mut reader, &shared.stop, &mut budget) {
                LineRead::Line(line) => line,
                LineRead::Closed => return,
                LineRead::TooLarge => {
                    refuse_head(&mut writer, &mut reader);
                    return;
                }
            };
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            headers += 1;
            if headers > MAX_HEADERS {
                refuse_head(&mut writer, &mut reader);
                return;
            }
            if let Some((k, v)) = line.split_once(':') {
                let v = v.trim();
                if k.eq_ignore_ascii_case("range") {
                    range = parse_range(v);
                } else if k.eq_ignore_ascii_case("connection") && v.eq_ignore_ascii_case("close") {
                    close = true;
                }
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        let body: Option<(Arc<Vec<u8>>, &str)> = if path.starts_with("/live.m3u8") {
            shared
                .playlist()
                .map(|p| (Arc::new(p.into_bytes()), "application/vnd.apple.mpegurl"))
        } else {
            path.strip_prefix("/s/")
                .and_then(|p| p.strip_suffix(".ts"))
                .and_then(|n| n.parse().ok())
                .and_then(|n| shared.segment(n))
                .map(|b| (b, "video/mp2t"))
        };
        let ok = match body {
            Some((bytes, ctype)) => write_body(&mut writer, &bytes, ctype, range),
            None => writer
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .is_ok(),
        };
        if !ok || close {
            return;
        }
    }
}

fn parse_range(v: &str) -> Option<(u64, Option<u64>)> {
    let spec = v.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let a = a.trim().parse().ok()?;
    let b = b.trim();
    Some((a, if b.is_empty() { None } else { b.parse().ok() }))
}

fn write_body(
    w: &mut TcpStream,
    bytes: &[u8],
    ctype: &str,
    range: Option<(u64, Option<u64>)>,
) -> bool {
    let len = u64::try_from(bytes.len()).unwrap_or(0);
    let (status, start, end) = match range {
        Some((a, b)) if a < len => (206, a, b.unwrap_or(len - 1).min(len - 1)),
        Some(_) if len > 0 => {
            let head = format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\n\r\n");
            return w.write_all(head.as_bytes()).is_ok();
        }
        _ => (200, 0, len.saturating_sub(1)),
    };
    let slice: &[u8] = if len == 0 {
        &[]
    } else {
        let (s, e) = (
            usize::try_from(start).unwrap_or(0),
            usize::try_from(end).unwrap_or(0),
        );
        bytes.get(s..=e).unwrap_or_default()
    };
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: keep-alive\r\n",
        if status == 206 { "Partial Content" } else { "OK" },
        slice.len()
    );
    if status == 206 {
        let _ = write!(head, "Content-Range: bytes {start}-{end}/{len}\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes()).is_ok() && w.write_all(slice).is_ok() && w.flush().is_ok()
}

#[cfg(test)]
#[allow(
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::indexing_slicing,
    clippy::format_push_string
)]
// reason: a compact fake origin and byte-range assertions in test code.
mod tests {
    use super::*;

    fn loopback_shared() -> Arc<Shared> {
        Arc::new(Shared {
            window: Mutex::new(Window {
                target_duration: 6.0,
                segments: VecDeque::new(),
                next_local: 0,
                failed: None,
            }),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
        })
    }

    fn loopback_client(shared: &Arc<Shared>) -> TcpStream {
        let listener = TcpListener::bind("[::1]:0").expect("bind");
        let address = listener.local_addr().expect("addr");
        spawn_server("bounds", listener, Arc::clone(shared)).expect("server");
        let client = TcpStream::connect(address).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client timeout");
        client
    }

    /// The whole response, or panic when the server never answers or closes.
    fn response_of(client: &mut TcpStream) -> String {
        let mut response = Vec::new();
        std::io::Read::read_to_end(client, &mut response).expect("server closed the connection");
        String::from_utf8_lossy(&response).into_owned()
    }

    #[test]
    fn an_unterminated_request_line_is_refused_at_its_bound() {
        let shared = loopback_shared();
        let mut client = loopback_client(&shared);
        client
            .write_all(&vec![b'a'; MAX_LINE_BYTES * 4])
            .expect("send");
        assert!(response_of(&mut client).starts_with("HTTP/1.1 431"));
        shared.stop.store(true, Ordering::Release);
    }

    #[test]
    fn an_unterminated_header_line_is_refused_at_its_bound() {
        let shared = loopback_shared();
        let mut client = loopback_client(&shared);
        client
            .write_all(b"GET /live.m3u8 HTTP/1.1\r\nX-Long: ")
            .expect("send");
        client
            .write_all(&vec![b'a'; MAX_LINE_BYTES * 4])
            .expect("send");
        assert!(response_of(&mut client).starts_with("HTTP/1.1 431"));
        shared.stop.store(true, Ordering::Release);
    }

    #[test]
    fn too_many_headers_are_refused() {
        let shared = loopback_shared();
        let mut client = loopback_client(&shared);
        let mut head = String::from("GET /live.m3u8 HTTP/1.1\r\n");
        for index in 0..=MAX_HEADERS {
            let _ = write!(head, "X-{index}: 1\r\n");
        }
        client.write_all(head.as_bytes()).expect("send");
        assert!(response_of(&mut client).starts_with("HTTP/1.1 431"));
        shared.stop.store(true, Ordering::Release);
    }

    #[test]
    fn many_legal_lines_cannot_exceed_the_head_budget() {
        let shared = loopback_shared();
        let mut client = loopback_client(&shared);
        let filler = "a".repeat(MAX_LINE_BYTES - 16);
        let mut head = String::from("GET /live.m3u8 HTTP/1.1\r\n");
        for index in 0..8 {
            let _ = write!(head, "X-{index}: {filler}\r\n");
        }
        client.write_all(head.as_bytes()).expect("send");
        assert!(response_of(&mut client).starts_with("HTTP/1.1 431"));
        shared.stop.store(true, Ordering::Release);
    }

    #[test]
    fn a_silent_connection_notices_shutdown() {
        let shared = loopback_shared();
        let mut client = loopback_client(&shared);
        client.write_all(b"GET /live").expect("partial request");
        std::thread::sleep(IO_POLL);
        shared.stop.store(true, Ordering::Release);
        assert_eq!(response_of(&mut client), "", "closed without a response");
    }

    const LIVE: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:30712\n#EXT-X-PROGRAM-DATE-TIME:2026-11-03T21:14:58.150Z\n#EXTINF:5.600,\na/1.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:5.600,\nhttps://cdn.example.net/2.ts?m=1\n";

    #[test]
    fn parses_a_live_media_playlist() {
        let p = parse_media_playlist(LIVE).expect("live playlist");
        assert!((p.target_duration - 6.0).abs() < f64::EPSILON);
        assert_eq!(p.segments.len(), 2);
        assert_eq!(p.segments[0].seq, 30712);
        assert_eq!(p.segments[0].uri, "a/1.ts");
        assert!(!p.segments[0].discontinuity);
        assert_eq!(p.segments[1].seq, 30713);
        assert!(p.segments[1].discontinuity);
        assert!((p.segments[1].duration - 5.6).abs() < 1e-9);
    }

    #[test]
    fn retains_original_cue_metadata_without_inventing_boundaries() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:41\n#EXT-X-ASSET:CAID=0x44495343\n#EXTINF:6,\na.ts\n";
        let p = parse_media_playlist(text).expect("live playlist");
        assert_eq!(p.cue_metadata.observations.len(), 1);
        assert_eq!(p.segments[0].seq, 41);
        assert_eq!(p.segments[0].uri, "a.ts");
    }

    #[test]
    fn original_metadata_observation_is_fetch_generation_fenced() {
        use crate::hls_event_observer::{HlsEventObserver, HlsEvidenceSink};
        use multiview_core::source_event::SourceEventObservation;
        struct Sink(Arc<Mutex<Vec<SourceEventObservation>>>);
        impl HlsEvidenceSink for Sink {
            fn try_observe(&mut self, o: SourceEventObservation) -> bool {
                self.0.lock().unwrap().push(o);
                true
            }
            fn mark_degraded(&mut self) {}
            fn retire(&mut self) {
                self.0.lock().unwrap().clear();
            }
        }
        let id = "observer-generation-test";
        let generation = begin_generation(id);
        let observations = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(Some(HlsEventObserver::new(
            id.into(),
            1,
            Sink(observations.clone()),
            8,
            256,
        ))));
        urls()
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .observation_active = Some(Arc::downgrade(&active));
        let playlist = parse_media_playlist(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-ASSET:CAID=original\n#EXTINF:6,\na.ts\n",
        )
        .unwrap();
        observe_playlist(&active, &AtomicBool::new(false), &playlist.cue_metadata);
        assert_eq!(
            observations.lock().unwrap()[0].asset_id.as_deref(),
            Some("original")
        );
        assert!(begin_generation(id) > generation);
        observe_playlist(
            &active,
            &AtomicBool::new(false),
            &multiview_input::hls::PlaylistCueMetadata::default(),
        );
        assert_eq!(observations.lock().unwrap().len(), 0);
    }

    #[test]
    fn incomplete_snapshots_are_not_absence_evidence() {
        for text in [
            "#EXTM3U\n",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n",
            "#EXTM3U\n#EXTINF:6,\na.ts\n#EXTINF:6,\n",
            "#EXTM3U\n#EXT-X-SKIP:SKIPPED-SEGMENTS=2\n#EXTINF:6,\na.ts\n",
        ] {
            assert!(parse_media_playlist(text).is_err(), "{text}");
        }
    }

    #[test]
    fn retirement_waits_for_batch_and_blocks_late_callbacks() {
        assert_retirement_fence(false);
    }

    #[test]
    fn drop_waits_for_batch_and_blocks_late_callbacks() {
        assert_retirement_fence(true);
    }

    fn assert_retirement_fence(drop_fetcher: bool) {
        use crate::hls_event_observer::{HlsEventObserver, HlsEvidenceSink};
        use multiview_core::source_event::SourceEventObservation;
        struct BlockingSink {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            evidence: Arc<Mutex<Vec<SourceEventObservation>>>,
        }
        impl HlsEvidenceSink for BlockingSink {
            fn try_observe(&mut self, o: SourceEventObservation) -> bool {
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
                self.evidence.lock().unwrap().push(o);
                true
            }
            fn mark_degraded(&mut self) {}
            fn retire(&mut self) {
                // Acquiring the URL lock here proves callbacks run outside it.
                // The test's bounded completion wait detects a lock-order deadlock.
                drop(urls().lock().unwrap());
                self.evidence.lock().unwrap().clear();
            }
        }
        let id = if drop_fetcher {
            "observer-drop-barrier-test"
        } else {
            "observer-barrier-test"
        };
        let generation = begin_generation(id);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let evidence = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Mutex::new(Some(HlsEventObserver::new(
            id.into(),
            1,
            BlockingSink {
                entered: entered_tx,
                release: release_rx,
                evidence: evidence.clone(),
            },
            8,
            256,
        ))));
        urls()
            .lock()
            .unwrap()
            .get_mut(id)
            .unwrap()
            .observation_active = Some(Arc::downgrade(&gate));
        let worker_gate = gate.clone();
        let worker = std::thread::spawn(move || {
            let metadata = multiview_input::hls::MediaPlaylist::parse_cue_metadata(
                "#EXTM3U\n#EXT-X-ASSET:CAID=A\na.ts\n#EXT-X-ASSET:CAID=B\nb.ts\n",
            )
            .unwrap();
            observe_playlist(&worker_gate, &AtomicBool::new(false), &metadata);
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let drop_gate = gate.clone();
        let retirement = std::thread::spawn(move || {
            if drop_fetcher {
                let fetcher = HlsFetcher {
                    shared: Arc::new(Shared {
                        window: Mutex::new(Window {
                            target_duration: 6.0,
                            segments: VecDeque::new(),
                            next_local: 0,
                            failed: None,
                        }),
                        ready: Condvar::new(),
                        stop: AtomicBool::new(false),
                    }),
                    source_id: id.into(),
                    local_url: "http://[::1]:1/live.m3u8".into(),
                    generation,
                    observation_gate: drop_gate,
                };
                drop(fetcher);
            } else {
                begin_generation(id);
            }
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_tx.send(()).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        retirement.join().unwrap();
        observe_playlist(
            &gate,
            &AtomicBool::new(false),
            &multiview_input::hls::PlaylistCueMetadata::default(),
        );
        assert!(evidence.lock().unwrap().is_empty());
        assert!(gate.lock().unwrap().is_none());
    }

    #[test]
    fn rejects_what_it_cannot_serve() {
        assert_eq!(parse_media_playlist("nope"), Err(Unsupported::NotAPlaylist));
        assert_eq!(
            parse_media_playlist("#EXTM3U\n#EXTINF:6,\na.ts\n#EXT-X-ENDLIST\n"),
            Err(Unsupported::Vod)
        );
        assert_eq!(
            parse_media_playlist(
                "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:6,\na.ts\n"
            ),
            Err(Unsupported::Feature("EXT-X-KEY"))
        );
        assert!(
            parse_media_playlist("#EXTM3U\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:6,\na.ts\n").is_ok()
        );
        assert_eq!(
            parse_media_playlist("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n"),
            Err(Unsupported::Master)
        );
    }

    #[test]
    fn ranges_cover_the_rest_exactly() {
        assert_eq!(split_ranges(0, 10, 3), vec![(0, 3), (4, 6), (7, 9)]);
        assert_eq!(
            split_ranges(65_536, 65_538, 6),
            vec![(65_536, 65_536), (65_537, 65_537)]
        );
        assert_eq!(split_ranges(5, 5, 6), Vec::new());
        let r = split_ranges(65_536, 780_000, 6);
        assert_eq!(r.first().map(|x| x.0), Some(65_536));
        assert_eq!(r.last().map(|x| x.1), Some(779_999));
        assert!(r.windows(2).all(|w| w[0].1 + 1 == w[1].0));
        assert_eq!(content_range_total("bytes 0-65535/780000"), Some(780_000));
    }

    fn listing(seqs: std::ops::Range<u64>) -> OriginPlaylist {
        OriginPlaylist {
            target_duration: 6.0,
            cue_metadata: multiview_input::hls::PlaylistCueMetadata::default(),
            segments: seqs
                .map(|seq| OriginSegment {
                    seq,
                    duration: 5.6,
                    discontinuity: false,
                    uri: format!("{seq}.ts"),
                })
                .collect(),
        }
    }

    #[test]
    fn schedule_starts_three_from_the_edge_and_takes_only_new_segments() {
        let mut sched = Schedule::default();
        assert_eq!(sched.absorb(listing(10..20)), Some(0));
        let seqs: Vec<u64> = sched.queued.iter().map(|s| s.seq).collect();
        assert_eq!(seqs, vec![17, 18, 19]);
        assert_eq!(sched.absorb(listing(11..21)), Some(0));
        assert_eq!(sched.queued.back().map(|s| s.seq), Some(20));
        assert_eq!(sched.queued.len(), 4, "no duplicates");
    }

    #[test]
    fn expired_undispatched_segments_count_as_lost_and_mark_a_gap() {
        let mut sched = Schedule::default();
        sched.absorb(listing(0..4));
        assert!(!sched.gap);
        // 1..=3 are queued; the playlist moved on past 1 and 2.
        assert_eq!(sched.absorb(listing(3..8)), Some(2));
        assert!(sched.gap);
        assert_eq!(sched.queued.front().map(|s| s.seq), Some(3));
        assert_eq!(sched.order.front().map(|s| s.seq), Some(3));
    }

    #[test]
    fn a_dispatched_segment_that_left_the_playlist_can_still_publish() {
        let mut sched = Schedule::default();
        sched.absorb(listing(0..3));
        // 0 is downloading (dispatched), 1 and 2 are queued.
        sched.queued.pop_front();
        assert_eq!(sched.absorb(listing(2..6)), Some(1), "only 1 is lost");
        sched.done.insert(0, Ok((vec![0], false, Duration::ZERO)));
        assert_eq!(sched.pop_ready().map(|(seg, _)| seg.seq), Some(0));
        assert!(sched.done.is_empty());
    }

    #[test]
    fn a_sequence_that_ran_backwards_is_refused() {
        let mut sched = Schedule::default();
        sched.absorb(listing(1_000..1_004));
        assert_eq!(sched.absorb(listing(0..4)), None);
    }

    #[test]
    fn segments_publish_in_order_whatever_order_they_finish() {
        let mut sched = Schedule::default();
        sched.absorb(listing(0..3));
        sched.done.insert(2, Ok((vec![2], false, Duration::ZERO)));
        assert!(sched.pop_ready().is_none(), "0 is not done");
        sched.done.insert(0, Ok((vec![0], false, Duration::ZERO)));
        assert_eq!(sched.pop_ready().map(|(seg, _)| seg.seq), Some(0));
        sched.done.insert(1, Err("gone".to_owned()));
        assert!(sched.pop_ready().is_some_and(|(_, r)| r.is_err()));
        assert_eq!(sched.pop_ready().map(|(seg, _)| seg.seq), Some(2));
    }

    /// A one-route origin answering every request with `respond(n)` (the
    /// request number), counting requests.
    fn scripted(respond: impl Fn(u64) -> Vec<u8> + Send + Sync + 'static) -> (u16, Arc<AtomicU64>) {
        let listener = TcpListener::bind("[::1]:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&hits);
        let respond = Arc::new(respond);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let count = Arc::clone(&count);
                let respond = Arc::clone(&respond);
                std::thread::spawn(move || {
                    let Ok(mut out) = stream.try_clone() else {
                        return;
                    };
                    let mut input = BufReader::new(stream);
                    loop {
                        let mut line = String::new();
                        if input.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        loop {
                            let mut header = String::new();
                            if input.read_line(&mut header).unwrap_or(0) == 0 {
                                return;
                            }
                            if header.trim_end().is_empty() {
                                break;
                            }
                        }
                        let n = count.fetch_add(1, Ordering::SeqCst);
                        if out.write_all(&respond(n)).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (port, hits)
    }

    fn reply(head: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!("{head}\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        v.extend_from_slice(body);
        v
    }

    /// bbc-news gzips its playlists even to clients that did not ask.
    #[test]
    fn a_gzipped_playlist_is_decoded() {
        let gz: Vec<u8> = vec![
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 83, 118, 141, 8, 241, 53, 14, 229, 82, 6, 210, 186,
            17, 186, 33, 142, 65, 238, 174, 33, 46, 161, 65, 142, 33, 158, 254, 126, 86, 166, 48,
            113, 95, 87, 23, 79, 71, 221, 96, 215, 192, 80, 87, 63, 103, 87, 43, 115, 176, 184,
            167, 159, 155, 149, 169, 158, 129, 14, 87, 162, 94, 73, 49, 23, 0, 175, 59, 19, 114,
            74, 0, 0, 0,
        ];
        let (port, _) = scripted(move |_| reply("HTTP/1.1 200 OK\r\nContent-Encoding: gzip", &gz));
        let (pl, _) = fetch_playlist(
            &agent(),
            &format!("http://[::1]:{port}/p.m3u8"),
            &Throttle::default(),
        )
        .expect("decoded");
        assert_eq!(pl.segments.len(), 1);
        assert_eq!(pl.segments[0].seq, 7);
    }

    #[test]
    fn a_429_pauses_requests_for_retry_after_and_halves_parallelism() {
        let (port, hits) = scripted(|n| {
            if n == 0 {
                reply("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1", b"")
            } else {
                reply("HTTP/1.1 200 OK", b"segment")
            }
        });
        let throttle = Throttle::default();
        assert_eq!(
            (throttle.parts(), throttle.in_flight()),
            (RANGE_PARTS, IN_FLIGHT)
        );
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        let (body, _) = fetch_segment(
            &agent(),
            &format!("http://[::1]:{port}/s.ts"),
            Instant::now() + Duration::from_secs(10),
            &stop,
            &throttle,
        )
        .expect("fetched after the pause");
        assert_eq!(body, b"segment");
        assert!(
            t0.elapsed() >= Duration::from_secs(1),
            "waited out Retry-After"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "no request during the pause"
        );
        assert_eq!(throttle.limited.load(Ordering::Relaxed), 1);
        assert_eq!((throttle.parts(), throttle.in_flight()), (3, 2));
        for _ in 0..CLEAN_TO_RECOVER {
            throttle.segment_ok();
        }
        assert_eq!(throttle.parts(), RANGE_PARTS, "clean segments restore it");
    }

    #[test]
    fn a_403_is_not_retried() {
        let (port, hits) = scripted(|_| reply("HTTP/1.1 403 Forbidden", b""));
        let stop = AtomicBool::new(false);
        let err = fetch_segment(
            &agent(),
            &format!("http://[::1]:{port}/s.ts"),
            Instant::now() + Duration::from_secs(10),
            &stop,
            &Throttle::default(),
        )
        .expect_err("forbidden");
        assert!(err.contains("403"), "{err}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "one request, not a retry storm"
        );
    }

    #[test]
    fn throttle_halves_at_most_once_per_pause_and_bottoms_out() {
        let throttle = Throttle::default();
        throttle.penalize(Some(Duration::from_secs(5)));
        throttle.penalize(Some(Duration::from_secs(5)));
        assert_eq!(throttle.parts(), 3, "one burst, one halving");
        *throttle.until.lock().expect("lock") = None;
        throttle.penalize(None);
        *throttle.until.lock().expect("lock") = None;
        throttle.penalize(Some(Duration::from_secs(3_600)));
        assert_eq!((throttle.parts(), throttle.in_flight()), (1, 1));
        assert!(
            throttle.remaining() <= THROTTLE_MAX,
            "a huge Retry-After is capped"
        );
        assert_eq!(retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }

    #[test]
    fn shared_url_requires_exact_lifecycle_and_old_drop_cannot_erase_new() {
        let shared = Arc::new(Shared {
            window: Mutex::new(Window {
                target_duration: 6.0,
                segments: VecDeque::new(),
                next_local: 0,
                failed: None,
            }),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let id = "shared-url-lifecycle-test";
        let a = begin_generation(id);
        let old = HlsFetcher {
            shared: Arc::clone(&shared),
            source_id: id.to_owned(),
            local_url: "http://[::1]:1/live.m3u8".to_owned(),
            generation: a,
            observation_gate: Arc::new(Mutex::new(None)),
        };
        publish_url(id, a, &old.local_url).expect("A publish");
        assert_eq!(shared_url(id, a).as_deref(), Some(old.local_url.as_str()));
        let b = begin_generation(id);
        assert_eq!(
            shared_url(id, b),
            None,
            "B starts before B fetch registration"
        );
        assert_eq!(shared_url(id, a), None, "A cannot be selected by B");
        let b_url = "http://[::1]:2/live.m3u8".to_owned();
        assert!(
            publish_url(id, a, &old.local_url).is_err(),
            "late A publish refused"
        );
        assert_eq!(shared_url(id, b), None);
        publish_url(id, b, &b_url).expect("B publish");
        drop(old);
        assert_eq!(shared_url(id, b), Some(b_url));
        assert!(shared.stop.load(Ordering::Acquire));
        urls().lock().expect("lock").remove(id);
    }

    /// A per-connection bandwidth-limited origin, like one TCP flow over the
    /// lossy 300 ms path from mda001: each connection sends at most
    /// `bytes_per_sec`, after `latency` per request.
    struct Origin {
        port: u16,
        hits: Arc<AtomicU64>,
    }

    fn origin(body: Vec<u8>, bytes_per_sec: usize, latency: Duration, ranges: bool) -> Origin {
        let listener = TcpListener::bind("[::1]:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let body = Arc::new(body);
        let hits = Arc::new(AtomicU64::new(0));
        let h = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let body = Arc::clone(&body);
                let hits = Arc::clone(&h);
                std::thread::spawn(move || {
                    let Ok(mut w) = stream.try_clone() else {
                        return;
                    };
                    let mut r = BufReader::new(stream);
                    loop {
                        let mut first = String::new();
                        if r.read_line(&mut first).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut range = None;
                        loop {
                            let mut l = String::new();
                            if r.read_line(&mut l).unwrap_or(0) == 0 {
                                return;
                            }
                            let l = l.trim_end().to_owned();
                            if l.is_empty() {
                                break;
                            }
                            if let Some(v) = l
                                .strip_prefix("Range: ")
                                .or_else(|| l.strip_prefix("range: "))
                            {
                                range = parse_range(v);
                            }
                        }
                        hits.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(latency);
                        let len = u64::try_from(body.len()).expect("len");
                        let (status, a, b) = match range {
                            Some((a, b)) if ranges => (206, a, b.unwrap_or(len - 1).min(len - 1)),
                            _ => (200, 0, len - 1),
                        };
                        let slice =
                            &body[usize::try_from(a).expect("a")..=usize::try_from(b).expect("b")];
                        let mut head =
                            format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n", slice.len());
                        if status == 206 {
                            head.push_str(&format!("Content-Range: bytes {a}-{b}/{len}\r\n"));
                        }
                        head.push_str("\r\n");
                        if w.write_all(head.as_bytes()).is_err() {
                            return;
                        }
                        for chunk in slice.chunks(bytes_per_sec / 20) {
                            if w.write_all(chunk).is_err() {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(50));
                        }
                    }
                });
            }
        });
        Origin { port, hits }
    }

    fn body(n: usize) -> Vec<u8> {
        (0..n).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
    }

    /// The mda001 condition: one flow at ~40 KB/s takes ~20 s for a 780 KB
    /// segment (0.3x real time for 5.6 s of media). Parallel ranges must fetch
    /// it intact several times faster.
    #[test]
    fn parallel_ranges_beat_a_slow_single_flow() {
        let data = body(780_000);
        let o = origin(data.clone(), 40_000, Duration::from_millis(150), true);
        let url = format!("http://[::1]:{}/seg.ts", o.port);
        let stop = AtomicBool::new(false);
        let t0 = Instant::now();
        let (got, ranged) = fetch_segment(
            &agent(),
            &url,
            Instant::now() + Duration::from_secs(60),
            &stop,
            &Throttle::default(),
        )
        .expect("fetch");
        let wall = t0.elapsed();
        assert!(ranged);
        assert_eq!(got, data, "reassembled intact");
        assert_eq!(o.hits.load(Ordering::SeqCst), 7, "probe + 6 ranges");
        // Single flow: ~19.5 s. Probe (~1.8 s) + 6 parallel (~3 s) ≈ 5 s.
        assert!(wall < Duration::from_secs(8), "took {wall:?}");
    }

    #[test]
    fn an_origin_without_ranges_is_fetched_whole() {
        let data = body(200_000);
        let o = origin(data.clone(), 2_000_000, Duration::ZERO, false);
        let url = format!("http://[::1]:{}/seg.ts", o.port);
        let stop = AtomicBool::new(false);
        let (got, ranged) = fetch_segment(
            &agent(),
            &url,
            Instant::now() + Duration::from_secs(20),
            &stop,
            &Throttle::default(),
        )
        .expect("fetch");
        assert!(!ranged);
        assert_eq!(got, data);
        assert_eq!(o.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_local_server_serves_the_window_with_ranges() {
        let shared = Arc::new(Shared {
            window: Mutex::new(Window {
                target_duration: 5.6,
                segments: VecDeque::new(),
                next_local: 0,
                failed: None,
            }),
            ready: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        for i in 0..(WINDOW + 2) {
            shared.publish(5.6, i == 3, vec![u8::try_from(i).unwrap_or(0); 10]);
        }
        let listener = TcpListener::bind("[::1]:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        spawn_server("t", listener, Arc::clone(&shared)).expect("server");
        let a = agent();
        let (s, _, pl) = get(
            &a,
            &format!("http://[::1]:{port}/live.m3u8"),
            None,
            &Throttle::default(),
        )
        .expect("playlist");
        assert_eq!(s, 200);
        let pl = String::from_utf8(pl).expect("utf8");
        assert!(pl.contains("#EXT-X-MEDIA-SEQUENCE:2\n"), "{pl}");
        assert!(pl.contains("#EXT-X-TARGETDURATION:6\n"), "{pl}");
        assert!(
            pl.contains("#EXT-X-DISCONTINUITY\n#EXTINF:5.600000,\n/s/3.ts"),
            "{pl}"
        );
        assert_eq!(pl.matches("#EXTINF").count(), WINDOW, "bounded window");
        shared.fail("origin gone".to_owned());
        let (gone, _, _) = get(
            &a,
            &format!("http://[::1]:{port}/live.m3u8"),
            None,
            &Throttle::default(),
        )
        .expect("gone");
        assert_eq!(
            gone, 404,
            "a failed fetcher fails libav's reload, forcing a reconnect"
        );
        shared.lock().failed = None;
        let (s, _, seg) = get(
            &a,
            &format!("http://[::1]:{port}/s/9.ts"),
            None,
            &Throttle::default(),
        )
        .expect("seg");
        assert_eq!((s, seg), (200, vec![9_u8; 10]));
        let (s, total, part) = get(
            &a,
            &format!("http://[::1]:{port}/s/9.ts"),
            Some((4, 99)),
            &Throttle::default(),
        )
        .expect("range");
        assert_eq!((s, total, part.len()), (206, Some(10), 6));
        let (s, _, _) = get(
            &a,
            &format!("http://[::1]:{port}/s/0.ts"),
            None,
            &Throttle::default(),
        )
        .expect("evicted");
        assert_eq!(s, 404, "evicted segments are gone");
        shared.stop.store(true, Ordering::Release);
    }
}
