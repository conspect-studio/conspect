//! The network half of the deep probe (feature `catalogue`): a guarded HTTP client
//! (every dial and every redirect hop screened, redirects followed manually, max 5),
//! a hard transfer budget, the stage pipeline, and — behind `catalogue-ffmpeg` —
//! the decode stage via `multiview-ffmpeg`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "catalogue-ffmpeg")]
use super::AudioTrack;
use super::{
    final_verdict, judge_liveness, parse_media, pick_variant, resolve_uri, status_verdict,
    BroadcastEvent, Liveness, MediaFacts, MediaInfo, ProbeRequest, ProbeResult, ProbeRunner, Stage,
    StageStatus, Verdict, MAX_BYTES, WALL_CLOCK,
};
use crate::catalogue::guard::DialGuard;
use crate::catalogue::sync::BoxFuture;

const MAX_REDIRECTS: usize = 5;
const MAX_PLAYLIST_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(feature = "catalogue-ffmpeg")]
const MAX_SEGMENTS_SAMPLED: usize = 3;

/// Why a fetch failed.
#[derive(Debug)]
enum FetchError {
    Blocked(String),
    Status(u16),
    Transport(String),
    Budget,
}

/// Shared transfer budget across one probe.
#[derive(Debug, Clone)]
struct Budget(Arc<AtomicU64>);

impl Budget {
    fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }
    fn add(&self, n: usize) -> bool {
        let n = u64::try_from(n).unwrap_or(u64::MAX);
        self.0.fetch_add(n, Ordering::AcqRel).saturating_add(n) <= MAX_BYTES
    }
    fn used(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// The production probe runner.
pub struct HttpProbeRunner {
    guard: DialGuard,
    client: reqwest::Client,
}

impl HttpProbeRunner {
    /// Build a runner screening every dial with `guard`.
    ///
    /// # Errors
    ///
    /// A message when the HTTP client cannot be built.
    pub fn new(guard: DialGuard) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(guard.resolver())
            .connect_timeout(Duration::from_secs(8))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { guard, client })
    }

    async fn get(
        &self,
        url: &str,
        req: &ProbeRequest,
        budget: &Budget,
        limit: u64,
    ) -> Result<(Vec<u8>, String), FetchError> {
        let mut current = url.to_owned();
        for _hop in 0..=MAX_REDIRECTS {
            let parsed =
                url::Url::parse(&current).map_err(|e| FetchError::Transport(e.to_string()))?;
            self.guard
                .screen_url(&parsed)
                .map_err(FetchError::Blocked)?;
            let mut rb = self
                .client
                .get(parsed.as_str())
                .header("Accept-Encoding", "identity");
            if let Some(ua) = &req.user_agent {
                rb = rb.header(reqwest::header::USER_AGENT, ua);
            }
            if let Some(r) = &req.referrer {
                rb = rb.header(reqwest::header::REFERER, r);
            }
            let mut resp = rb.send().await.map_err(|e| {
                let text = e.to_string();
                let chain = std::iter::successors(std::error::Error::source(&e), |s| s.source())
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(": ");
                if chain.contains("blocked")
                    || chain.contains("loopback")
                    || chain.contains("link-local")
                {
                    FetchError::Blocked(chain)
                } else {
                    FetchError::Transport(text)
                }
            })?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| resolve_uri(&current, l))
                    .ok_or(FetchError::Status(status))?;
                current = loc;
                continue;
            }
            if status != 200 {
                return Err(FetchError::Status(status));
            }
            if resp.content_length().is_some_and(|n| n > limit) {
                return Err(FetchError::Budget);
            }
            let mut body = Vec::new();
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if !budget.add(chunk.len())
                            || u64::try_from(body.len() + chunk.len()).unwrap_or(u64::MAX) > limit
                        {
                            return Err(FetchError::Budget);
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) => return Err(FetchError::Transport(e.to_string())),
                }
            }
            return Ok((body, current));
        }
        Err(FetchError::Transport("too many redirects".to_owned()))
    }
}

fn stage(name: &'static str, status: StageStatus, detail: impl Into<Option<String>>) -> Stage {
    Stage {
        name,
        status,
        detail: detail.into(),
    }
}

fn blank(url: &str) -> ProbeResult {
    ProbeResult {
        id: String::new(),
        state: "done",
        url: url.to_owned(),
        verdict: None,
        media_checked: false,
        verdict_basis: "stages 1-3 (playlist liveness)",
        stages: Vec::new(),
        facts: MediaFacts::default(),
        events: Vec::new(),
        probed_from: String::new(),
        probed_at: None,
        bytes: 0,
        cached: false,
        network: None,
    }
}

fn fail(mut r: ProbeResult, e: &FetchError, stage_name: &'static str) -> ProbeResult {
    let (verdict, detail) = match e {
        FetchError::Blocked(m) => (Verdict::Blocked, format!("refused by the dial guard: {m}")),
        FetchError::Status(s) => (
            status_verdict(*s).unwrap_or(Verdict::Dead),
            format!("HTTP {s}"),
        ),
        FetchError::Transport(m) => (Verdict::Dead, m.clone()),
        FetchError::Budget => (
            Verdict::Dead,
            "exceeded the 16 MiB transfer budget".to_owned(),
        ),
    };
    if matches!(e, FetchError::Blocked(_)) {
        r.stages.push(stage("guard", StageStatus::Failed, detail));
    } else {
        r.stages.push(stage("guard", StageStatus::Ok, None));
        r.stages
            .push(stage(stage_name, StageStatus::Failed, detail));
    }
    r.verdict = Some(verdict);
    r.verdict_basis = "failed before media";
    r
}

impl HttpProbeRunner {
    #[allow(clippy::too_many_lines)]
    async fn probe(&self, req: ProbeRequest) -> ProbeResult {
        let budget = Budget::new();
        let mut result = blank(&req.url);
        // Stage 1+2: playlist (guard is applied inside every fetch hop).
        let (body, base) = match self.get(&req.url, &req, &budget, MAX_PLAYLIST_BYTES).await {
            Ok(v) => v,
            Err(e) => {
                let mut r = fail(result, &e, "playlist");
                r.bytes = budget.used();
                return r;
            }
        };
        let text = String::from_utf8_lossy(&body).into_owned();
        result.stages.push(stage("guard", StageStatus::Ok, None));
        let (media_url, media_text, advertised) =
            if let Some((uri, bw, vtt)) = pick_variant(&text, req.quality.as_deref()) {
                result.facts.advertised_bps = bw;
                result.facts.webvtt_renditions = vtt;
                let Some(variant_url) = resolve_uri(&base, &uri) else {
                    result.stages.push(stage(
                        "playlist",
                        StageStatus::Failed,
                        "unresolvable variant uri".to_owned(),
                    ));
                    result.verdict = Some(Verdict::Dead);
                    return result;
                };
                match self
                    .get(&variant_url, &req, &budget, MAX_PLAYLIST_BYTES)
                    .await
                {
                    Ok((b, final_url)) => (final_url, String::from_utf8_lossy(&b).into_owned(), bw),
                    Err(e) => {
                        let mut r = fail(result, &e, "playlist");
                        r.bytes = budget.used();
                        return r;
                    }
                }
            } else {
                (base, text, None)
            };
        let Some(first) = parse_media(&media_text) else {
            result.stages.push(stage(
                "playlist",
                StageStatus::Failed,
                "not an HLS media playlist".to_owned(),
            ));
            result.verdict = Some(Verdict::Dead);
            result.bytes = budget.used();
            return result;
        };
        if first.segments.is_empty() {
            result.stages.push(stage(
                "playlist",
                StageStatus::Failed,
                "playlist lists no segments".to_owned(),
            ));
            result.verdict = Some(Verdict::Dead);
            result.bytes = budget.used();
            return result;
        }
        result.stages.push(stage(
            "playlist",
            StageStatus::Ok,
            Some(format!("{} segments", first.segments.len())),
        ));
        // Stage 3: liveness — two reloads spaced by (capped) target duration.
        let mut reloads = vec![first];
        let spacing = reloads
            .first()
            .map_or(2.0, |m| m.target_duration)
            .clamp(1.0, 4.0);
        for _ in 0..2 {
            tokio::time::sleep(Duration::from_secs_f64(spacing)).await;
            if let Ok((b, _)) = self
                .get(&media_url, &req, &budget, MAX_PLAYLIST_BYTES)
                .await
            {
                if let Some(m) = parse_media(&String::from_utf8_lossy(&b)) {
                    let advanced = reloads
                        .last()
                        .is_some_and(|l| m.media_sequence > l.media_sequence);
                    reloads.push(m);
                    if advanced {
                        break;
                    }
                }
            }
        }
        let now_unix = crate::catalogue::store::now_unix();
        let liveness = judge_liveness(&reloads, now_unix);
        result.stages.push(stage(
            "liveness",
            if liveness == Liveness::Live {
                StageStatus::Ok
            } else {
                StageStatus::Failed
            },
            Some(format!("{liveness:?}")),
        ));
        // Playlist events from the newest reload.
        let newest = reloads.last().cloned().unwrap_or_default();
        for (kind, detail) in &newest.events {
            result.events.push(BroadcastEvent {
                kind,
                source: "playlist",
                detail: detail.clone(),
            });
        }
        // Stage 4: media.
        let (media_checked, decoded_video, decoded_h) = self
            .media_stage(&req, &media_url, &newest, advertised, &budget, &mut result)
            .await;
        result.media_checked = media_checked;
        result.verdict_basis = if media_checked {
            "stages 1-4 (playlist, liveness and decoded media)"
        } else {
            "stages 1-3 only (playlist liveness); media decode not available in this build"
        };
        result.verdict = Some(final_verdict(
            liveness,
            media_checked,
            decoded_video,
            decoded_h,
            req.quality.as_deref(),
        ));
        result.bytes = budget.used();
        result
    }

    #[cfg(not(feature = "catalogue-ffmpeg"))]
    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    async fn media_stage(
        &self,
        _req: &ProbeRequest,
        _media_url: &str,
        _newest: &MediaInfo,
        _advertised: Option<u64>,
        _budget: &Budget,
        result: &mut ProbeResult,
    ) -> (bool, bool, Option<u32>) {
        result.stages.push(stage(
            "media",
            StageStatus::Skipped,
            Some("this build has no libav decode stage (feature `catalogue-ffmpeg`)".to_owned()),
        ));
        result
            .stages
            .push(stage("facts", StageStatus::Skipped, None));
        result.stages.push(stage(
            "events",
            StageStatus::Ok,
            Some(format!("{} playlist events", result.events.len())),
        ));
        (false, false, None)
    }

    #[cfg(feature = "catalogue-ffmpeg")]
    #[allow(clippy::too_many_lines)] // linear fetch→decode→fold pipeline; helpers already split out
    async fn media_stage(
        &self,
        req: &ProbeRequest,
        media_url: &str,
        newest: &MediaInfo,
        advertised: Option<u64>,
        budget: &Budget,
        result: &mut ProbeResult,
    ) -> (bool, bool, Option<u32>) {
        if newest.encrypted {
            result.stages.push(stage(
                "media",
                StageStatus::Skipped,
                Some("segments are encrypted; decode not attempted".to_owned()),
            ));
            result
                .stages
                .push(stage("facts", StageStatus::Skipped, None));
            result.stages.push(stage(
                "events",
                StageStatus::Ok,
                Some(format!("{} playlist events", result.events.len())),
            ));
            return (false, false, None);
        }
        let n = newest.segments.len().min(MAX_SEGMENTS_SAMPLED);
        let offset = newest.segments.len().saturating_sub(n);
        let picks = newest.segments.get(offset..).unwrap_or(&[]);
        let init = match newest
            .init
            .as_deref()
            .and_then(|u| resolve_uri(media_url, u))
        {
            Some(u) => self
                .get(&u, req, budget, MAX_BYTES)
                .await
                .ok()
                .map(|(b, _)| b),
            None => None,
        };
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        let mut seg_bytes: u64 = 0;
        let mut seg_millis: u64 = 0;
        for (i, seg) in picks.iter().enumerate() {
            let Some(url) = resolve_uri(media_url, seg) else {
                continue;
            };
            match self.get(&url, req, budget, MAX_BYTES).await {
                Ok((b, _)) => {
                    seg_bytes += u64::try_from(b.len()).unwrap_or(0);
                    seg_millis += newest.durations.get(offset + i).map_or(0, |d| {
                        #[allow(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            clippy::as_conversions
                        )]
                        {
                            (d.max(0.0) * 1000.0) as u64
                        }
                    });
                    let mut c = init.clone().unwrap_or_default();
                    c.extend_from_slice(&b);
                    chunks.push(c);
                }
                Err(FetchError::Blocked(m)) => {
                    result.stages.push(stage(
                        "media",
                        StageStatus::Failed,
                        Some(format!("segment refused by the dial guard: {m}")),
                    ));
                    return (true, false, None);
                }
                Err(e) => {
                    result.stages.push(stage(
                        "media",
                        StageStatus::Failed,
                        Some(format!("segment fetch failed: {e:?}")),
                    ));
                    return (true, false, None);
                }
            }
        }
        // Track durations as integer milliseconds so bits/s needs no float cast.
        result.facts.measured_bps = seg_bytes.saturating_mul(8_000).checked_div(seg_millis);
        let _ = advertised;
        let decode = tokio::task::spawn_blocking(move || {
            multiview_ffmpeg::media_probe::probe_segments(&chunks, Duration::from_secs(8))
        })
        .await;
        let sample = match decode {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                result
                    .stages
                    .push(stage("media", StageStatus::Failed, Some(e.to_string())));
                return (true, false, None);
            }
            Err(e) => {
                result.stages.push(stage(
                    "media",
                    StageStatus::Failed,
                    Some(format!("decode worker: {e}")),
                ));
                return (true, false, None);
            }
        };
        let video_ok = apply_sample(&sample, result);
        (true, video_ok, sample.video.first().map(|v| v.height))
    }
}

/// Fold a decoded [`multiview_ffmpeg::media_probe::MediaSample`] into the result's
/// stages, facts and events; returns whether video was decoded.
#[cfg(feature = "catalogue-ffmpeg")]
fn apply_sample(
    sample: &multiview_ffmpeg::media_probe::MediaSample,
    result: &mut ProbeResult,
) -> bool {
    use base64::Engine as _;
    let video_ok = sample.video_frames >= 1;
    let audio_ok = sample.audio_ms >= 100 || sample.audio.is_empty();
    let ok = video_ok && audio_ok;
    result.stages.push(stage(
        "media",
        if ok {
            StageStatus::Ok
        } else {
            StageStatus::Failed
        },
        Some(format!(
            "{} video frame(s), {} ms audio decoded",
            sample.video_frames, sample.audio_ms
        )),
    ));
    if let Some(v) = sample.video.first() {
        result.facts.video_codec = Some(v.codec.clone());
        result.facts.width = Some(v.width);
        result.facts.height = Some(v.height);
        #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
        {
            result.facts.fps = v
                .fps
                .filter(|(_, d)| *d > 0)
                .map(|(n, d)| n as f64 / d as f64);
        }
    }
    result.facts.audio = sample
        .audio
        .iter()
        .map(|a| AudioTrack {
            codec: a.codec.clone(),
            sample_rate: a.sample_rate,
            channels: a.channels,
            language: a.language.clone(),
        })
        .collect();
    result.facts.cea_captions = sample.has_cea_captions;
    if let Some(t) = &sample.thumb {
        use multiview_preview::JpegEncoder as _;
        if let Ok(jpeg) =
            multiview_preview::Nv12JpegEncoder::new().encode_nv12(&t.plane, t.width, t.height, 70)
        {
            result.facts.thumbnail_jpeg_base64 =
                Some(base64::engine::general_purpose::STANDARD.encode(jpeg));
        }
    }
    result.stages.push(stage("facts", StageStatus::Ok, None));
    for sec in &sample.scte35_sections {
        let detail = multiview_input::scte::splice35::SpliceInfoSection::parse(sec)
            .ok()
            .and_then(|s| s.cue_event())
            .map(|c| format!("{c:?}"));
        result.events.push(BroadcastEvent {
            kind: "inband_scte35",
            source: "segment",
            detail,
        });
    }
    for _ in 0..sample.id3_packets.min(8) {
        result.events.push(BroadcastEvent {
            kind: "id3",
            source: "segment",
            detail: None,
        });
    }
    result.stages.push(stage(
        "events",
        StageStatus::Ok,
        Some(format!("{} events", result.events.len())),
    ));
    video_ok
}

impl ProbeRunner for HttpProbeRunner {
    fn run(&self, _id: String, req: ProbeRequest) -> BoxFuture<'_, ProbeResult> {
        Box::pin(async move {
            let started = Instant::now();
            let mut r = self.probe(req).await;
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis(),
                wall = WALL_CLOCK.as_secs(),
                "probe finished"
            );
            r.state = "done";
            r
        })
    }
}
