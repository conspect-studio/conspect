//! Deep probe (docs/io/iptv-org.md §3, R3): prove a stream **actually delivers live
//! media from this server**, not merely that a root URL answers.
//!
//! Stages: 1 guard (every dial + redirect screened) → 2 playlist (UA/Referer
//! replayed, master → variant) → 3 liveness (two reloads, media sequence must
//! advance, PDT near wall clock, no ENDLIST) → 4 media (2–3 newest segments demuxed
//! **and decoded** via `multiview-ffmpeg`, behind `catalogue-ffmpeg`; otherwise
//! reported `skipped` and the verdict is stage 1–3 only) → 5 facts (+ thumbnail) →
//! 6 broadcast events (playlist cue tags, in-band SCTE-35, ID3, discontinuities;
//! reported, never a failure).
//!
//! Bounds (all enforced, all chaos-tested): 4 concurrent probes (semaphore), queue
//! of 64 (overflow ⇒ 429, never blocks), 20 s wall clock, 16 MiB transfer, 60
//! probes/min per principal, 15-min result cache keyed by URL + headers. Runs on
//! the control-plane runtime; decode runs on `spawn_blocking`. Nothing here is
//! reachable from the data plane (invariants #1/#10).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::error::ControlError;

/// Concurrent probes per process.
pub const MAX_CONCURRENT: usize = 4;
/// Queued probes beyond the running ones before `429`.
pub const MAX_QUEUED: usize = 64;
/// Wall clock per probe.
pub const WALL_CLOCK: Duration = Duration::from_secs(20);
/// Transfer cap per probe (playlists + segments).
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Probes per principal per minute.
pub const PER_PRINCIPAL_PER_MINUTE: usize = 60;
/// Result cache lifetime.
pub const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
/// Cap on stored finished probes.
const MAX_STORED: usize = 512;

/// The probe verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Verdict {
    /// Plays and is live.
    Live,
    /// Plays but not live / stalled / far below advertised quality.
    Degraded,
    /// 403/451 from this host.
    GeoBlockedHere,
    /// 404/410/DNS/timeout.
    Dead,
    /// Playlist fine but no decodable media.
    NoMedia,
    /// Refused by the dial guard.
    Blocked,
}

/// Stage outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// Passed.
    Ok,
    /// Failed.
    Failed,
    /// Not run (earlier failure or capability absent).
    Skipped,
}

/// One stage's report.
#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    /// Stage name (`guard`, `playlist`, `liveness`, `media`, `facts`, `events`).
    pub name: &'static str,
    /// Outcome.
    pub status: StageStatus,
    /// Short human detail.
    pub detail: Option<String>,
}

/// Media facts (stage 5).
#[derive(Debug, Clone, Default, Serialize)]
pub struct MediaFacts {
    /// Video codec.
    pub video_codec: Option<String>,
    /// Width.
    pub width: Option<u32>,
    /// Height.
    pub height: Option<u32>,
    /// Frame rate.
    pub fps: Option<f64>,
    /// Measured bitrate (bits/s) over the sampled segments.
    pub measured_bps: Option<u64>,
    /// Advertised `BANDWIDTH` of the chosen variant.
    pub advertised_bps: Option<u64>,
    /// Audio tracks.
    pub audio: Vec<AudioTrack>,
    /// CEA-608/708 captions seen in decoded video.
    pub cea_captions: bool,
    /// `WebVTT` subtitle renditions declared.
    pub webvtt_renditions: u32,
    /// Base64 JPEG thumbnail (320 px wide), when decoded.
    pub thumbnail_jpeg_base64: Option<String>,
}

/// An audio track.
#[derive(Debug, Clone, Serialize)]
pub struct AudioTrack {
    /// Codec.
    pub codec: String,
    /// Sample rate.
    pub sample_rate: u32,
    /// Channels.
    pub channels: u16,
    /// Language.
    pub language: Option<String>,
}

/// One broadcast event (stage 6).
#[derive(Debug, Clone, Serialize)]
pub struct BroadcastEvent {
    /// One of `cue_out`, `cue_out_cont`, `cue_in`, `daterange_scte35`,
    /// `oatcls_scte35`, `inband_scte35`, `id3`, `discontinuity`.
    pub kind: &'static str,
    /// Where it was seen (`playlist` / `segment`).
    pub source: &'static str,
    /// Detail (duration, id, splice summary).
    pub detail: Option<String>,
}

/// A finished (or running) probe job.
#[derive(Debug, Clone, Serialize)]
pub struct ProbeResult {
    /// Job id.
    pub id: String,
    /// `queued`, `running` or `done`.
    pub state: &'static str,
    /// Probed URL.
    pub url: String,
    /// Verdict (`done` only).
    pub verdict: Option<Verdict>,
    /// Whether stage 4 ran; `false` ⇒ the verdict rests on stages 1–3 only.
    pub media_checked: bool,
    /// Human label of the evidence behind the verdict.
    pub verdict_basis: &'static str,
    /// Per-stage reports.
    pub stages: Vec<Stage>,
    /// Facts.
    pub facts: MediaFacts,
    /// Broadcast events.
    pub events: Vec<BroadcastEvent>,
    /// Host that probed (geo-blocking depends on where the server is).
    pub probed_from: String,
    /// RFC 3339 completion time.
    pub probed_at: Option<String>,
    /// Bytes transferred.
    pub bytes: u64,
    /// Served from the 15-minute cache.
    pub cached: bool,
    /// Network-path report (§3a), when diagnostics ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<super::netdiag::NetworkReport>,
}

/// Request to probe.
#[derive(Debug, Clone)]
pub struct ProbeRequest {
    /// Stream URL.
    pub url: String,
    /// `User-Agent` to replay.
    pub user_agent: Option<String>,
    /// `Referer` to replay.
    pub referrer: Option<String>,
    /// Advertised quality (`720p`), used to pick the variant and judge degradation.
    pub quality: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure analysis (always compiled, unit-tested offline).
// ---------------------------------------------------------------------------

/// Resolve a playlist `uri` against `base` (absolute, protocol-relative, root- or
/// path-relative). Pure string logic; the result is screened before any dial.
#[must_use]
pub fn resolve_uri(base: &str, uri: &str) -> Option<String> {
    let uri = uri.trim();
    if uri.is_empty() {
        return None;
    }
    if uri.contains("://") {
        return Some(uri.to_owned());
    }
    let scheme_end = base.find("://")?;
    let (scheme, rest) = (&base[..scheme_end], &base[scheme_end + 3..]);
    if let Some(stripped) = uri.strip_prefix("//") {
        return Some(format!("{scheme}://{stripped}"));
    }
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = (&rest[..authority_end], &rest[authority_end..]);
    if uri.starts_with('/') {
        return Some(format!("{scheme}://{authority}{uri}"));
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    Some(format!("{scheme}://{authority}{dir}{uri}"))
}

/// A parsed media playlist reduced to what the probe needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MediaInfo {
    /// `#EXT-X-MEDIA-SEQUENCE`.
    pub media_sequence: u64,
    /// `#EXT-X-TARGETDURATION` seconds.
    pub target_duration: f64,
    /// Whether `#EXT-X-ENDLIST` is present (⇒ VOD).
    pub endlist: bool,
    /// Segment URIs in order.
    pub segments: Vec<String>,
    /// Segment durations in seconds.
    pub durations: Vec<f64>,
    /// `#EXT-X-MAP` init section URI (fMP4), if any.
    pub init: Option<String>,
    /// Whether segments are AES-encrypted (`#EXT-X-KEY` not `NONE`).
    pub encrypted: bool,
    /// Number of `#EXT-X-DISCONTINUITY` tags.
    pub discontinuities: u32,
    /// Latest `#EXT-X-PROGRAM-DATE-TIME` (unix seconds).
    pub last_pdt_unix: Option<i64>,
    /// Playlist-level cue / marker events.
    pub events: Vec<(&'static str, Option<String>)>,
}

fn parse_pdt(v: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v.trim())
        .ok()
        .map(|d| d.timestamp())
}

/// Parse a media playlist. `None` when it is not a media playlist.
#[must_use]
pub fn parse_media(text: &str) -> Option<MediaInfo> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return None;
    }
    let mut info = MediaInfo::default();
    let mut dur: Option<f64> = None;
    for (n, line) in lines.enumerate() {
        if n > 100_000 {
            break;
        }
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            info.media_sequence = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            info.target_duration = v.trim().parse().unwrap_or(0.0);
        } else if line == "#EXT-X-ENDLIST" {
            info.endlist = true;
        } else if line.starts_with("#EXT-X-STREAM-INF") {
            return None;
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            dur = v.split(',').next().and_then(|d| d.trim().parse().ok());
        } else if line == "#EXT-X-DISCONTINUITY" {
            info.discontinuities += 1;
            info.events.push(("discontinuity", None));
        } else if let Some(v) = line.strip_prefix("#EXT-X-PROGRAM-DATE-TIME:") {
            info.last_pdt_unix = parse_pdt(v).or(info.last_pdt_unix);
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            info.init = v.split(',').find_map(|kv| {
                kv.trim()
                    .strip_prefix("URI=")
                    .map(|u| u.trim_matches('"').to_owned())
            });
        } else if let Some(v) = line.strip_prefix("#EXT-X-KEY:") {
            if !v.contains("METHOD=NONE") {
                info.encrypted = true;
            }
        } else if let Some(v) = line.strip_prefix("#EXT-X-CUE-OUT-CONT:") {
            info.events
                .push(("cue_out_cont", Some(v.trim().to_owned())));
        } else if let Some(v) = line.strip_prefix("#EXT-X-CUE-OUT") {
            let d = v.trim_start_matches(':').trim();
            info.events
                .push(("cue_out", (!d.is_empty()).then(|| d.to_owned())));
        } else if line.starts_with("#EXT-X-CUE-IN") {
            info.events.push(("cue_in", None));
        } else if let Some(v) = line.strip_prefix("#EXT-OATCLS-SCTE35:") {
            info.events.push((
                "oatcls_scte35",
                Some(format!("{} base64 chars", v.trim().len())),
            ));
        } else if let Some(v) = line.strip_prefix("#EXT-X-DATERANGE:") {
            if v.contains("SCTE35-") {
                let id = v
                    .split(',')
                    .find_map(|kv| kv.trim().strip_prefix("ID="))
                    .map(|s| s.trim_matches('"').to_owned());
                let which = ["SCTE35-OUT", "SCTE35-IN", "SCTE35-CMD"]
                    .iter()
                    .find(|k| v.contains(*k))
                    .copied()
                    .unwrap_or("SCTE35");
                info.events.push((
                    "daterange_scte35",
                    Some(format!(
                        "{which}{}",
                        id.map(|i| format!(" id={i}")).unwrap_or_default()
                    )),
                ));
            }
        } else if !line.starts_with('#') {
            info.segments.push(line.to_owned());
            info.durations.push(dur.take().unwrap_or(0.0));
        }
    }
    Some(info)
}

/// Pick the variant URI closest to `quality` (`720p`); highest bandwidth otherwise.
/// Returns `(uri, advertised_bandwidth, webvtt_rendition_count)`.
#[must_use]
pub fn pick_variant(master: &str, quality: Option<&str>) -> Option<(String, Option<u64>, u32)> {
    let m = multiview_input::hls::MasterPlaylist::parse(master).ok()?;
    let target = quality.and_then(|q| q.trim_end_matches(['p', 'i']).parse::<u32>().ok());
    let v = m.pick_video_variant(target)?;
    let vtt = u32::try_from(m.subtitle_renditions().count()).unwrap_or(0);
    Some((v.uri.clone(), v.bandwidth, vtt))
}

/// Liveness judgement from two (or more) successive reloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Sequence advanced (and PDT, if any, is recent).
    Live,
    /// `#EXT-X-ENDLIST`: a VOD, not live.
    NotLive,
    /// Sequence did not advance across reloads.
    Stalled,
    /// Sequence advanced but PDT is more than 5 minutes off the wall clock.
    StalePdt,
}

/// Judge liveness from successive reloads and the wall clock (unix seconds).
#[must_use]
pub fn judge_liveness(reloads: &[MediaInfo], now_unix: i64) -> Liveness {
    let Some(last) = reloads.last() else {
        return Liveness::Stalled;
    };
    if last.endlist {
        return Liveness::NotLive;
    }
    let advanced = reloads.windows(2).any(|w| match w {
        [a, b] => {
            b.media_sequence > a.media_sequence
                || (b.segments.last() != a.segments.last() && !b.segments.is_empty())
        }
        _ => false,
    });
    if reloads.len() >= 2 && !advanced {
        return Liveness::Stalled;
    }
    if last
        .last_pdt_unix
        .is_some_and(|p| (now_unix - p).abs() > 300)
    {
        return Liveness::StalePdt;
    }
    Liveness::Live
}

/// Map an HTTP status of the playlist fetch to a verdict.
#[must_use]
pub fn status_verdict(status: u16) -> Option<Verdict> {
    match status {
        200..=299 => None,
        403 | 451 => Some(Verdict::GeoBlockedHere),
        _ => Some(Verdict::Dead),
    }
}

/// Parse an advertised `720p`/`1080i` into a height.
fn advertised_height(q: Option<&str>) -> Option<u32> {
    q?.trim_end_matches(['p', 'i']).parse().ok()
}

/// Final verdict from stage outcomes.
#[must_use]
pub fn final_verdict(
    liveness: Liveness,
    media_checked: bool,
    decoded_video: bool,
    decoded_height: Option<u32>,
    quality: Option<&str>,
) -> Verdict {
    if media_checked && !decoded_video {
        return Verdict::NoMedia;
    }
    match liveness {
        Liveness::NotLive | Liveness::Stalled | Liveness::StalePdt => Verdict::Degraded,
        Liveness::Live => {
            if let (Some(adv), Some(got)) = (advertised_height(quality), decoded_height) {
                if got.saturating_mul(2) < adv {
                    return Verdict::Degraded;
                }
            }
            Verdict::Live
        }
    }
}

// ---------------------------------------------------------------------------
// Service: bounded queue + rate limit + cache. Always compiled; the probe body
// (network) is behind `catalogue`.
// ---------------------------------------------------------------------------

/// Executes one probe. Production: [`engine::HttpProbeRunner`]; tests inject fakes
/// (e.g. one that never finishes) to prove the queue/timeout bounds.
pub trait ProbeRunner: Send + Sync {
    /// Run the probe, honouring `WALL_CLOCK` itself or being cancelled by it.
    fn run(&self, id: String, req: ProbeRequest) -> super::sync::BoxFuture<'_, ProbeResult>;
}

/// One member of a batch probe job.
#[derive(Debug, Clone)]
struct JobEntry {
    k: Option<String>,
    source_id: Option<String>,
    probe_id: String,
}

/// What a batch probe member was addressed by.
#[derive(Debug, Clone)]
pub struct JobTarget {
    /// Index row key, when probed by key.
    pub k: Option<String>,
    /// Linked source id, when probed by source.
    pub source_id: Option<String>,
    /// The resolved request (server-owned; the browser never supplies a URL).
    pub request: ProbeRequest,
}

#[derive(Default)]
struct Shared {
    results: HashMap<String, ProbeResult>,
    order: VecDeque<String>,
    cache: HashMap<String, (Instant, String)>,
    jobs: HashMap<String, Vec<JobEntry>>,
    /// Submitting principal (key id) per job.
    job_owner: HashMap<String, String>,
    job_order: VecDeque<String>,
    rate: HashMap<String, VecDeque<Instant>>,
}

/// The bounded probe service.
pub struct ProbeService {
    runner: Arc<dyn ProbeRunner>,
    permits: Arc<tokio::sync::Semaphore>,
    /// Probes admitted (running + queued); admission fails at `MAX_CONCURRENT + MAX_QUEUED`.
    admitted: Arc<std::sync::atomic::AtomicUsize>,
    shared: Arc<Mutex<Shared>>,
    host: String,
    collector: Option<Arc<dyn super::netdiag::NetworkCollector>>,
}

impl std::fmt::Debug for ProbeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeService").finish_non_exhaustive()
    }
}

/// RAII admission slot: released on completion and on cancellation/drop.
struct AdmissionSlot(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// A probe permit plus its admission slot, held for the duration of a
/// diagnostics run.
pub struct DiagnosticsPermit {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _slot: AdmissionSlot,
}

fn cache_key(r: &ProbeRequest) -> String {
    format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        r.url,
        r.user_agent.as_deref().unwrap_or(""),
        r.referrer.as_deref().unwrap_or(""),
        r.quality.as_deref().unwrap_or("")
    )
}

impl ProbeService {
    /// A service over `runner`; `host` is reported as `probed_from`.
    #[must_use]
    pub fn new(runner: Arc<dyn ProbeRunner>, host: String) -> Arc<Self> {
        Arc::new(Self {
            runner,
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            admitted: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shared: Arc::new(Mutex::new(Shared::default())),
            host,
            collector: None,
        })
    }

    /// A service whose finished results also carry the §3a `network` block,
    /// collected under the same permit (so the same bounds) after the probe.
    #[must_use]
    pub fn with_network(
        runner: Arc<dyn ProbeRunner>,
        host: String,
        collector: Arc<dyn super::netdiag::NetworkCollector>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runner,
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT)),
            admitted: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shared: Arc::new(Mutex::new(Shared::default())),
            host,
            collector: Some(collector),
        })
    }

    /// Reserve one admission slot atomically; `None` when the queue is full.
    fn try_reserve(&self) -> Option<AdmissionSlot> {
        use std::sync::atomic::Ordering;
        self.admitted
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CONCURRENT + MAX_QUEUED).then_some(n + 1)
            })
            .ok()
            .map(|_| AdmissionSlot(Arc::clone(&self.admitted)))
    }

    /// Acquire a probe permit (the same semaphore the probes use) for work that
    /// must obey the probe bounds, such as an on-demand network report. The
    /// admission slot is reserved before waiting and released when the returned
    /// permit is dropped or the waiting future is cancelled; the per-principal
    /// budget is shared with [`Self::submit`].
    ///
    /// # Errors
    ///
    /// [`ControlError::TooManyRequests`] when the principal's per-minute budget or
    /// the probe queue is exhausted.
    pub async fn acquire_for_diagnostics(
        &self,
        principal: &str,
    ) -> Result<DiagnosticsPermit, ControlError> {
        let now = Instant::now();
        let slot = {
            let mut g = self.lock();
            let q = g.rate.entry(principal.to_owned()).or_default();
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
            {
                q.pop_front();
            }
            if q.len() >= PER_PRINCIPAL_PER_MINUTE {
                return Err(ControlError::TooManyRequests(format!(
                    "probe rate limit of {PER_PRINCIPAL_PER_MINUTE}/min reached"
                )));
            }
            let slot = self.try_reserve().ok_or_else(|| {
                ControlError::TooManyRequests("probe queue is full; retry shortly".to_owned())
            })?;
            g.rate
                .entry(principal.to_owned())
                .or_default()
                .push_back(now);
            slot
        };
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| ControlError::Conflict("probe service closed".to_owned()))?;
        Ok(DiagnosticsPermit {
            _permit: permit,
            _slot: slot,
        })
    }

    /// The network collector, when diagnostics are enabled.
    #[must_use]
    pub fn collector(&self) -> Option<&Arc<dyn super::netdiag::NetworkCollector>> {
        self.collector.as_ref()
    }

    /// The reported probing host.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn store(shared: &mut Shared, result: ProbeResult) {
        let id = result.id.clone();
        if shared.results.insert(id.clone(), result).is_none() {
            shared.order.push_back(id);
        }
        while shared.order.len() > MAX_STORED {
            if let Some(old) = shared.order.pop_front() {
                shared.results.remove(&old);
            }
        }
    }

    /// Submit a probe for `principal`. Returns the job (possibly a cache hit).
    ///
    /// # Errors
    ///
    /// [`ControlError::TooManyRequests`] when the principal's per-minute budget or
    /// the global queue is exhausted (never blocks), [`ControlError::Validation`]
    /// for an unusable URL.
    #[allow(clippy::too_many_lines)] // one admission path: cache, rate limit, queue
    pub fn submit(
        self: &Arc<Self>,
        principal: &str,
        req: ProbeRequest,
    ) -> Result<ProbeResult, ControlError> {
        if !(req.url.starts_with("http://") || req.url.starts_with("https://")) {
            return Err(ControlError::Validation(
                "only http(s) stream URLs can be probed".to_owned(),
            ));
        }
        let key = cache_key(&req);
        let now = Instant::now();
        let slot = {
            let mut g = self.lock();
            let q = g.rate.entry(principal.to_owned()).or_default();
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60))
            {
                q.pop_front();
            }
            if q.len() >= PER_PRINCIPAL_PER_MINUTE {
                return Err(ControlError::TooManyRequests(format!(
                    "probe rate limit of {PER_PRINCIPAL_PER_MINUTE}/min reached"
                )));
            }
            if let Some((at, id)) = g.cache.get(&key).cloned() {
                if now.duration_since(at) < CACHE_TTL {
                    if let Some(hit) = g.results.get(&id).cloned() {
                        if hit.state == "done" {
                            let mut hit = hit;
                            hit.cached = true;
                            return Ok(hit);
                        }
                        // Already running: hand back the in-flight job.
                        return Ok(hit);
                    }
                }
            }
            let Some(reserved) = self.try_reserve() else {
                return Err(ControlError::TooManyRequests(
                    "probe queue is full; retry shortly".to_owned(),
                ));
            };
            let q = g.rate.entry(principal.to_owned()).or_default();
            q.push_back(now);
            reserved
        };
        let id = uuid::Uuid::new_v4().to_string();
        let queued = ProbeResult {
            id: id.clone(),
            state: "queued",
            url: req.url.clone(),
            verdict: None,
            media_checked: false,
            verdict_basis: "pending",
            stages: Vec::new(),
            facts: MediaFacts::default(),
            events: Vec::new(),
            probed_from: self.host.clone(),
            probed_at: None,
            bytes: 0,
            cached: false,
            network: None,
        };
        {
            let mut g = self.lock();
            Self::store(&mut g, queued.clone());
            g.cache.insert(key.clone(), (now, id.clone()));
            if g.cache.len() > 4 * MAX_STORED {
                g.cache
                    .retain(|_, (t, _)| now.duration_since(*t) < CACHE_TTL);
            }
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let _slot = slot;
            let _permit = me.permits.acquire().await;
            {
                let mut g = me.lock();
                if let Some(r) = g.results.get_mut(&id) {
                    r.state = "running";
                }
            }
            let net_req = req.clone();
            let fut = me.runner.run(id.clone(), req);
            let mut result = match tokio::time::timeout(WALL_CLOCK, fut).await {
                Ok(r) => r,
                Err(_) => timeout_result(&id, &me.host),
            };
            if let Some(c) = &me.collector {
                // Own 15 s budget inside the collector; still under this permit.
                result.network = Some(c.collect(net_req, None).await);
            }
            result.id.clone_from(&id);
            result.state = "done";
            result.probed_from.clone_from(&me.host);
            result.probed_at = Some(super::store::now_rfc3339());
            {
                let mut g = me.lock();
                Self::store(&mut g, result);
            }
        });
        Ok(queued)
    }

    /// A job by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<ProbeResult> {
        self.lock().results.get(id).cloned()
    }

    /// Submit a batch (<= 50). Each member goes through [`Self::submit`] (same
    /// semaphore, queue, rate limit and cache); the returned id addresses the
    /// batch.
    ///
    /// # Errors
    ///
    /// The first member's submit error (429, validation).
    pub fn submit_job(
        self: &Arc<Self>,
        principal: &str,
        targets: Vec<JobTarget>,
    ) -> Result<String, ControlError> {
        let mut entries = Vec::with_capacity(targets.len());
        for t in targets {
            let r = self.submit(principal, t.request)?;
            entries.push(JobEntry {
                k: t.k,
                source_id: t.source_id,
                probe_id: r.id,
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut g = self.lock();
        g.jobs.insert(id.clone(), entries);
        g.job_owner.insert(id.clone(), principal.to_owned());
        g.job_order.push_back(id.clone());
        while g.job_order.len() > MAX_STORED {
            if let Some(old) = g.job_order.pop_front() {
                g.jobs.remove(&old);
                g.job_owner.remove(&old);
            }
        }
        Ok(id)
    }

    /// The submitting principal and linked source ids of a job, or `None`
    /// for an unknown id (for per-object authorization on read).
    #[must_use]
    pub fn job_access(&self, id: &str) -> Option<(String, Vec<String>)> {
        let g = self.lock();
        let entries = g.jobs.get(id)?;
        let owner = g.job_owner.get(id).cloned().unwrap_or_default();
        Some((
            owner,
            entries.iter().filter_map(|e| e.source_id.clone()).collect(),
        ))
    }

    /// A batch job as the web reads it, or `None` for an unknown id.
    #[must_use]
    pub fn job(&self, id: &str) -> Option<wire::ProbeJob> {
        let g = self.lock();
        let entries = g.jobs.get(id)?;
        let mut results = Vec::new();
        let mut pending = 0_usize;
        let mut running = 0_usize;
        for e in entries {
            match g.results.get(&e.probe_id) {
                Some(r) if r.state == "done" => {
                    results.push(wire::to_wire(r, e.k.clone(), e.source_id.clone()));
                }
                Some(r) => {
                    pending += 1;
                    if r.state == "running" {
                        running += 1;
                    }
                }
                None => {
                    // Evicted from the bounded store before it was read.
                    results.push(wire::evicted(e.k.clone(), e.source_id.clone(), &self.host));
                }
            }
        }
        let state = if pending == 0 {
            "done"
        } else if running > 0 || !results.is_empty() {
            "running"
        } else {
            "queued"
        };
        Some(wire::ProbeJob {
            id: id.to_owned(),
            state,
            results,
            error: None,
        })
    }

    /// Latest finished result for a URL+headers (used by the link tracker).
    #[must_use]
    pub fn latest_for(&self, req: &ProbeRequest) -> Option<ProbeResult> {
        let g = self.lock();
        let (_, id) = g.cache.get(&cache_key(req))?;
        g.results.get(id).cloned()
    }

    /// Probes currently admitted (running + queued) — chaos-test observable.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.admitted.load(std::sync::atomic::Ordering::Acquire)
    }
}

fn timeout_result(id: &str, host: &str) -> ProbeResult {
    ProbeResult {
        id: id.to_owned(),
        state: "done",
        url: String::new(),
        verdict: Some(Verdict::Dead),
        media_checked: false,
        verdict_basis: "timeout",
        stages: vec![Stage {
            name: "playlist",
            status: StageStatus::Failed,
            detail: Some(format!(
                "probe exceeded the {}s wall clock",
                WALL_CLOCK.as_secs()
            )),
        }],
        facts: MediaFacts::default(),
        events: Vec::new(),
        probed_from: host.to_owned(),
        probed_at: None,
        bytes: 0,
        cached: false,
        network: None,
    }
}

#[cfg(feature = "catalogue")]
pub mod engine;

/// The web contract's probe job/result shapes (`ProbeJob`, `ProbeResult`,
/// `ProbeFacts`, `ProbeEvent`).
pub mod wire {
    use serde::Serialize;

    use super::{ProbeResult as Internal, Verdict};

    /// `ProbeEvent`.
    #[derive(Debug, Clone, Serialize)]
    pub struct ProbeEvent {
        /// Event kind.
        pub kind: String,
        /// Position in the sampled window. The probe records where an event was
        /// seen (playlist/segment), not a time, so this is always `null`.
        pub at: Option<String>,
        /// Detail.
        pub detail: Option<String>,
    }

    /// `ProbeFacts`.
    #[derive(Debug, Clone, Serialize)]
    pub struct ProbeFacts {
        /// Width.
        pub width: Option<u32>,
        /// Height.
        pub height: Option<u32>,
        /// Frame rate.
        pub fps: Option<f64>,
        /// Video codec.
        pub video_codec: Option<String>,
        /// First audio track's codec.
        pub audio_codec: Option<String>,
        /// Measured bitrate.
        pub bitrate_kbps: Option<u64>,
        /// Advertised `BANDWIDTH`.
        pub advertised_kbps: Option<u64>,
        /// Audio languages.
        pub audio_languages: Vec<String>,
        /// Caption kinds seen (`cea608/708`, `webvtt`).
        pub captions: Vec<String>,
        /// `data:image/jpeg;base64,…` thumbnail.
        pub thumbnail: Option<String>,
    }

    /// `ProbeResult`.
    #[derive(Debug, Clone, Serialize)]
    pub struct ProbeResult {
        /// Index key probed.
        pub k: Option<String>,
        /// Linked source probed.
        pub source_id: Option<String>,
        /// Verdict.
        pub verdict: Verdict,
        /// Probing host.
        pub probed_from: String,
        /// Completion time.
        pub probed_at: String,
        /// Short human detail.
        pub detail: Option<String>,
        /// Media facts.
        pub facts: Option<ProbeFacts>,
        /// Broadcast events.
        pub events: Vec<ProbeEvent>,
        /// Network-path report (§3a).
        #[serde(skip_serializing_if = "Option::is_none")]
        pub network: Option<crate::catalogue::netdiag::NetworkReport>,
    }

    /// `ProbeJob`.
    #[derive(Debug, Clone, Serialize)]
    pub struct ProbeJob {
        /// Job id.
        pub id: String,
        /// `queued`, `running`, `done`, `failed`.
        pub state: &'static str,
        /// Finished results.
        pub results: Vec<ProbeResult>,
        /// Job-level error.
        pub error: Option<String>,
    }

    pub(super) fn evicted(k: Option<String>, source_id: Option<String>, host: &str) -> ProbeResult {
        ProbeResult {
            k,
            source_id,
            verdict: Verdict::Dead,
            probed_from: host.to_owned(),
            probed_at: crate::catalogue::store::now_rfc3339(),
            detail: Some("the result was evicted before it was read".to_owned()),
            facts: None,
            events: Vec::new(),
            network: None,
        }
    }

    /// Convert a finished internal result.
    #[must_use]
    pub fn to_wire(r: &Internal, k: Option<String>, source_id: Option<String>) -> ProbeResult {
        let f = &r.facts;
        let has_facts = f.video_codec.is_some() || !f.audio.is_empty() || f.width.is_some();
        let mut captions = Vec::new();
        if f.cea_captions {
            captions.push("cea608/708".to_owned());
        }
        if f.webvtt_renditions > 0 {
            captions.push("webvtt".to_owned());
        }
        let detail = r
            .stages
            .iter()
            .find(|s| s.status == super::StageStatus::Failed)
            .and_then(|s| s.detail.clone())
            .or_else(|| Some(r.verdict_basis.to_owned()));
        ProbeResult {
            k,
            source_id,
            verdict: r.verdict.unwrap_or(Verdict::Dead),
            probed_from: r.probed_from.clone(),
            probed_at: r
                .probed_at
                .clone()
                .unwrap_or_else(crate::catalogue::store::now_rfc3339),
            detail,
            facts: has_facts.then(|| ProbeFacts {
                width: f.width,
                height: f.height,
                fps: f.fps,
                video_codec: f.video_codec.clone(),
                audio_codec: f.audio.first().map(|a| a.codec.clone()),
                bitrate_kbps: f.measured_bps.map(|b| b / 1000),
                advertised_kbps: f.advertised_bps.map(|b| b / 1000),
                audio_languages: f.audio.iter().filter_map(|a| a.language.clone()).collect(),
                captions,
                thumbnail: f
                    .thumbnail_jpeg_base64
                    .as_ref()
                    .map(|b| format!("data:image/jpeg;base64,{b}")),
            }),
            events: r
                .events
                .iter()
                .map(|e| ProbeEvent {
                    kind: e.kind.to_owned(),
                    at: None,
                    detail: e.detail.clone(),
                })
                .collect(),
            network: r.network.clone(),
        }
    }
}

/// A runner that reports the probe client could not be built.
#[derive(Debug, Default)]
pub struct UnavailableRunner;

impl ProbeRunner for UnavailableRunner {
    fn run(&self, _id: String, req: ProbeRequest) -> super::sync::BoxFuture<'_, ProbeResult> {
        Box::pin(async move {
            let mut r = timeout_result("", "");
            r.url = req.url;
            r.verdict_basis = "probe client unavailable";
            if let Some(s) = r.stages.first_mut() {
                s.detail = Some("the probe HTTP client could not be built".to_owned());
            }
            r
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_uris() {
        assert_eq!(
            resolve_uri("https://h/a/b/m.m3u8?x=1", "seg1.ts").unwrap(),
            "https://h/a/b/seg1.ts"
        );
        assert_eq!(
            resolve_uri("https://h/a/b/m.m3u8", "/z/s.ts").unwrap(),
            "https://h/z/s.ts"
        );
        assert_eq!(
            resolve_uri("https://h/a/m.m3u8", "//c/s.ts").unwrap(),
            "https://c/s.ts"
        );
        assert_eq!(
            resolve_uri("https://h/a/m.m3u8", "http://o/s.ts").unwrap(),
            "http://o/s.ts"
        );
        assert!(resolve_uri("https://h/a", "").is_none());
    }

    const LIVE: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:100\n#EXT-X-PROGRAM-DATE-TIME:2026-11-07T05:00:00Z\n#EXTINF:6.0,\ns100.ts\n#EXT-X-CUE-OUT:30\n#EXTINF:6.0,\ns101.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:6.0,\ns102.ts\n#EXT-X-CUE-IN\n#EXT-X-DATERANGE:ID=\"ad1\",START-DATE=\"2026-11-07T05:00:00Z\",SCTE35-OUT=0xFC30\n#EXT-OATCLS-SCTE35:/DAlAAAA\n";

    #[test]
    fn parses_events_and_discontinuities() {
        let m = parse_media(LIVE).unwrap();
        assert_eq!(m.media_sequence, 100);
        assert_eq!(m.segments, vec!["s100.ts", "s101.ts", "s102.ts"]);
        assert_eq!(m.discontinuities, 1);
        let kinds: Vec<&str> = m.events.iter().map(|e| e.0).collect();
        assert_eq!(
            kinds,
            vec![
                "cue_out",
                "discontinuity",
                "cue_in",
                "daterange_scte35",
                "oatcls_scte35"
            ]
        );
        assert!(!m.endlist && !m.encrypted);
    }

    #[test]
    fn master_is_not_media_and_endlist_is_not_live() {
        assert!(parse_media("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n").is_none());
        let vod = parse_media("#EXTM3U\n#EXTINF:6,\na.ts\n#EXT-X-ENDLIST\n").unwrap();
        assert_eq!(judge_liveness(&[vod], 0), Liveness::NotLive);
    }

    #[test]
    fn liveness_needs_advancing_sequence_and_recent_pdt() {
        let a = parse_media(LIVE).unwrap();
        let mut b = a.clone();
        assert_eq!(
            judge_liveness(&[a.clone(), b.clone()], 1_794_000_000),
            Liveness::Stalled
        );
        b.media_sequence = 101;
        b.segments.push("s103.ts".into());
        let pdt = a.last_pdt_unix.unwrap();
        assert_eq!(
            judge_liveness(&[a.clone(), b.clone()], pdt + 10),
            Liveness::Live
        );
        assert_eq!(judge_liveness(&[a, b], pdt + 4000), Liveness::StalePdt);
    }

    #[test]
    fn verdict_rules() {
        assert_eq!(status_verdict(403), Some(Verdict::GeoBlockedHere));
        assert_eq!(status_verdict(451), Some(Verdict::GeoBlockedHere));
        assert_eq!(status_verdict(404), Some(Verdict::Dead));
        assert_eq!(status_verdict(200), None);
        assert_eq!(
            final_verdict(Liveness::Live, true, false, None, None),
            Verdict::NoMedia
        );
        assert_eq!(
            final_verdict(Liveness::Live, false, false, None, None),
            Verdict::Live
        );
        assert_eq!(
            final_verdict(Liveness::Stalled, true, true, Some(720), None),
            Verdict::Degraded
        );
        assert_eq!(
            final_verdict(Liveness::Live, true, true, Some(240), Some("1080p")),
            Verdict::Degraded
        );
        assert_eq!(
            final_verdict(Liveness::Live, true, true, Some(720), Some("1080p")),
            Verdict::Live
        );
    }

    #[test]
    fn picks_variant_nearest_quality() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\nmid.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080\nhi.m3u8\n";
        let (uri, bw, _) = pick_variant(master, Some("720p")).unwrap();
        assert_eq!((uri.as_str(), bw), ("mid.m3u8", Some(3_000_000)));
    }

    struct Instant0;
    impl ProbeRunner for Instant0 {
        fn run(
            &self,
            id: String,
            _: ProbeRequest,
        ) -> super::super::sync::BoxFuture<'_, ProbeResult> {
            Box::pin(async move { timeout_result(&id, "h") })
        }
    }

    fn req(quality: Option<&str>) -> ProbeRequest {
        ProbeRequest {
            url: "https://a/x.m3u8".into(),
            user_agent: Some("UA".into()),
            referrer: None,
            quality: quality.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn cache_key_separates_quality() {
        let svc = ProbeService::new(Arc::new(Instant0), "h".into());
        let a = svc.submit("p", req(Some("720p"))).unwrap();
        let a2 = svc.submit("p", req(Some("720p"))).unwrap();
        let b = svc.submit("p", req(Some("1080p"))).unwrap();
        assert_eq!(a.id, a2.id, "same quality shares the cache entry");
        assert_ne!(
            a.id, b.id,
            "different quality must not share a cached verdict"
        );
    }

    #[tokio::test]
    async fn diagnostics_queue_is_capped_and_cancellation_releases_slots() {
        let svc = ProbeService::new(Arc::new(Instant0), "h".into());
        let mut held = Vec::new();
        for i in 0..MAX_CONCURRENT {
            held.push(svc.acquire_for_diagnostics(&format!("h{i}")).await.unwrap());
        }
        let mut waiters = Vec::new();
        for i in 0..MAX_QUEUED {
            let s = Arc::clone(&svc);
            waiters.push(tokio::spawn(async move {
                s.acquire_for_diagnostics(&format!("w{i}"))
                    .await
                    .map(|_| ())
            }));
        }
        for _ in 0..10_000 {
            if svc.admitted.load(std::sync::atomic::Ordering::Acquire)
                == MAX_CONCURRENT + MAX_QUEUED
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let over = svc.acquire_for_diagnostics("over").await;
        assert!(
            matches!(over, Err(ControlError::TooManyRequests(_))),
            "cap reached"
        );
        // Dropping (cancelling) a waiter frees its admission slot.
        let w = waiters.pop().unwrap();
        w.abort();
        let _ = w.await;
        let again = tokio::time::timeout(
            Duration::from_millis(50),
            svc.acquire_for_diagnostics("again"),
        )
        .await;
        assert!(again.is_err(), "admitted and now waiting, not rejected");
        drop(held);
    }

    #[tokio::test]
    async fn diagnostics_share_the_per_principal_rate_limit() {
        let svc = ProbeService::new(Arc::new(Instant0), "h".into());
        for _ in 0..PER_PRINCIPAL_PER_MINUTE {
            drop(svc.acquire_for_diagnostics("p").await.unwrap());
        }
        let r = svc.acquire_for_diagnostics("p").await;
        assert!(matches!(r, Err(ControlError::TooManyRequests(_))));
        assert!(svc.acquire_for_diagnostics("other").await.is_ok());
    }
}
