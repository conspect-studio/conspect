//! The ladder call's per-stream state and its strict body validation
//! (ADR-0114 decision 3, "The capability probe and the ladder endpoint").
//!
//! `POST /cast/<token>/ladder` carries a receiver's capability probe (or,
//! every 30 s, a clock-only call). Every field is a claim by an
//! unauthenticated device: it is used only to leave rungs out and to show
//! the owner what the device can do, never to widen access. Validation is
//! strict where a type is wrong (`400`) and ignores fields it does not know.
//!
//! Bounded, like the report: one [`LadderState`] per stream (at most
//! [`super::MAX_STREAMS`]), strings cut or refused at fixed lengths, at most
//! [`MAX_H264_PROFILES`] H.264 profiles, [`MAX_SKIP`] skip entries and
//! [`MAX_FALLBACKS`] fallbacks.

use std::collections::VecDeque;

use serde::Serialize;
use serde_json::{Map, Value};

/// Largest ladder body (bytes of JSON).
pub const MAX_LADDER_BYTES: usize = 2048;
/// Ladder calls per stream: burst.
pub const LADDER_BURST: u32 = 4;
/// Ladder calls per stream: one more every this many milliseconds.
pub const LADDER_REFILL_MS: u64 = 1000;
/// `caps.rtc_h264` entries kept.
pub const MAX_H264_PROFILES: usize = 8;
/// `skip` entries kept.
pub const MAX_SKIP: usize = 4;
/// Fallbacks remembered per stream (oldest forgotten first).
pub const MAX_FALLBACKS: usize = 8;
/// `receiver.ua` characters.
pub const MAX_UA: usize = 200;
/// `receiver.build` characters.
pub const MAX_BUILD: usize = 64;
/// Characters of a short string field (`receiver.kind`, `caps.srt`, a rung
/// name, a profile id, `need.audio`, `resets.last`).
pub const MAX_SHORT: usize = 32;
/// Largest `skip` duration (seconds).
pub const MAX_SKIP_SECONDS: u64 = 86_400;
/// Largest millisecond figure a receiver may report (10 minutes).
pub const MAX_REPORTED_MS: u64 = 600_000;
/// Largest latency a receiver may report (ms; its own sanity bound is 60 s).
pub const MAX_LATENCY_MS: u64 = 60_000;
/// Largest `t0` accepted (microseconds since the Unix epoch, year 2286).
pub const MAX_T0_US: f64 = 1.0e16;
/// Every `failed.stage` the protocol knows (decision 3).
pub const STAGES: &[&str] = &["connect", "first-frame", "stall", "latency", "hook"];
/// Rungs that need the receiver's presented-frame counter (`caps.frames`)
/// for their stall watchdog: left out when it is absent ("Cast media
/// status", rule 1).
pub const NEED_FRAME_COUNTER: &[&str] = &["moq", "webrtc"];
/// Every rung name the protocol knows (decision 2). Only `hls` is served
/// by this build.
pub const RUNGS: &[&str] = &["srt", "moq", "webrtc", "hls"];

/// A decoder check result (`isConfigSupported`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Support {
    /// The configuration is supported.
    Supported,
    /// The API exists and refused the configuration.
    Unsupported,
    /// The API does not exist.
    Absent,
}

/// Which presented-frame counter the receiver's stall watchdog uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum FrameCounter {
    /// `requestVideoFrameCallback`.
    Rvfc,
    /// `getVideoPlaybackQuality().totalVideoFrames`.
    Quality,
}

/// How a latency figure was obtained (decision 2, "When there is no
/// trustworthy mapping").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum LatencyMethod {
    /// Per presented frame.
    Frame,
    /// From the playhead, sampled every 100 ms.
    Playhead,
    /// No trustworthy mapping: no figure.
    Unavailable,
}

/// What the receiver says it can do (`caps` of the probe).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReceiverCaps {
    /// `WebTransport` exists.
    pub wt: Option<bool>,
    /// Media Source Extensions accept the stream's codec strings.
    pub mse: Option<bool>,
    /// `VideoDecoder.isConfigSupported` for the stream.
    pub vdec: Option<Support>,
    /// The same with `prefer-hardware`.
    pub vdec_hw: Option<bool>,
    /// `AudioDecoder.isConfigSupported` for the stream.
    pub adec: Option<Support>,
    /// `RTCPeerConnection` exists.
    pub rtc: Option<bool>,
    /// H.264 `profile-level-id` values the receiver can take (at most 8).
    pub rtc_h264: Vec<String>,
    /// Opus receive.
    pub rtc_opus: Option<bool>,
    /// `requestVideoFrameCallback` exists.
    pub rvfc: Option<bool>,
    /// The presented-frame counter (`null`: none).
    pub frames: Option<FrameCounter>,
    /// The libsrt version an Android TV app loaded (`null` on the web).
    pub srt: Option<String>,
}

/// What the session must carry (`need` of the probe).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReceiverNeed {
    /// Subtitles are required.
    pub subtitles: Option<bool>,
    /// The audio track required, if any.
    pub audio: Option<String>,
}

/// A rung in the device's failure memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SkipEntry {
    /// The rung.
    pub rung: String,
    /// Seconds left.
    pub seconds: u64,
}

/// The last capability probe of a stream's receiver.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReceiverProbe {
    /// `web` or `android-tv` (as claimed).
    pub kind: Option<String>,
    /// The receiver build (a sha256 prefix of its script).
    pub build: Option<String>,
    /// The receiver's user agent (at most 200 characters).
    pub ua: Option<String>,
    /// What it can do.
    pub caps: ReceiverCaps,
    /// What it must carry.
    pub need: ReceiverNeed,
    /// Rungs in its failure memory (at most 4).
    pub skip: Vec<SkipEntry>,
    /// When the probe arrived (Unix ms).
    pub received_unix_ms: u64,
}

/// One step down the ladder, as the receiver reported it in `failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Fallback {
    /// The rung that failed.
    pub rung: String,
    /// `connect`, `first-frame`, `stall`, `latency` or `hook` (the WebRTC
    /// media-element hook did not take).
    pub stage: String,
    /// The receiver's figure for it (ms), if any.
    pub ms: Option<u64>,
    /// When it was reported (Unix ms).
    pub at_unix_ms: u64,
}

/// Steady-state tick-to-present latency over the receiver's last window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LatencyStats {
    /// Median (ms).
    pub median: u64,
    /// 95th percentile (ms).
    pub p95: u64,
    /// Valid samples in the window.
    pub samples: u64,
    /// `frame` or `playhead`.
    pub method: LatencyMethod,
    /// The clock offset's error bound (ms).
    pub clock_error_ms: Option<u64>,
}

/// Player resets inside the attachment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Resets {
    /// Soft resets.
    pub soft: u64,
    /// Hard resets.
    pub hard: u64,
    /// The last cause.
    pub last: Option<String>,
}

/// The transport a stream's receiver plays on and how it fares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Transport {
    /// The rung the engine last offered (`hls` in this build).
    pub rung: Option<String>,
    /// Since when it plays that rung (Unix ms).
    pub since_unix_ms: Option<u64>,
    /// The last steps down (at most 8, oldest first).
    pub fallbacks: Vec<Fallback>,
    /// From the start of the attempt to the first presented frame (ms).
    pub first_frame_ms: Option<u64>,
    /// Steady-state latency; `null` when unavailable or not yet measured.
    pub latency_ms: Option<LatencyStats>,
    /// How the latency is measured (`unavailable`: no figure, by rule).
    pub latency_method: Option<LatencyMethod>,
    /// Player resets.
    pub resets: Resets,
    /// The receiver's frame watchdog says no frame was presented for 2 s.
    pub stalled: bool,
    /// When the receiver last reported these figures (Unix ms).
    pub reported_unix_ms: Option<u64>,
}

/// The per-stream ladder state kept in the registry.
#[derive(Debug, Clone)]
pub(super) struct LadderState {
    /// Rate limiter: `(tokens, last refill ms)`.
    pub bucket: Option<(u32, u64)>,
    pub probe: Option<ReceiverProbe>,
    pub transport: Option<Transport>,
    pub fallbacks: VecDeque<Fallback>,
    /// Live push attachments (`moq`, `webrtc`, `srt`; none in this build). While
    /// any exists a successful ladder call or report counts as a playlist
    /// fetch for the token's liveness (decision 5.5).
    pub push_attachments: u32,
}

impl Default for LadderState {
    fn default() -> Self {
        Self {
            bucket: None,
            probe: None,
            transport: None,
            fallbacks: VecDeque::with_capacity(MAX_FALLBACKS),
            push_attachments: 0,
        }
    }
}

impl LadderState {
    /// Take one call from the bucket (burst [`LADDER_BURST`], one more per
    /// [`LADDER_REFILL_MS`]). `false`: over the rate.
    pub(super) fn admit(&mut self, now: u64) -> bool {
        let (tokens, at) = self.bucket.get_or_insert((LADDER_BURST, now));
        let gained = now.saturating_sub(*at) / LADDER_REFILL_MS;
        let refilled = u64::from(*tokens).saturating_add(gained);
        *tokens = u32::try_from(refilled.min(u64::from(LADDER_BURST))).unwrap_or(LADDER_BURST);
        *at = if *tokens == LADDER_BURST {
            now
        } else {
            at.saturating_add(gained.saturating_mul(LADDER_REFILL_MS))
        };
        if *tokens == 0 {
            return false;
        }
        *tokens -= 1;
        true
    }

    /// The transport as shown (fallbacks folded in).
    pub(super) fn transport_view(&self) -> Option<Transport> {
        let mut transport = self.transport.clone()?;
        transport.fallbacks = self.fallbacks.iter().cloned().collect();
        Some(transport)
    }

    /// Record an answered ladder call that offered `rung`.
    pub(super) fn offered(&mut self, rung: &str, failed: Option<Fallback>, now: u64) {
        let transport = self.transport.get_or_insert_with(Transport::default);
        if transport.rung.as_deref() != Some(rung) {
            transport.rung = Some(rung.to_owned());
            transport.since_unix_ms = Some(now);
        }
        if let Some(fallback) = failed {
            while self.fallbacks.len() >= MAX_FALLBACKS {
                self.fallbacks.pop_front();
            }
            self.fallbacks.push_back(fallback);
        }
    }

    /// Fold the `transport` part of a receiver report in.
    pub(super) fn reported(&mut self, report: TransportReport, now: u64) {
        let transport = self.transport.get_or_insert_with(Transport::default);
        transport.first_frame_ms = report.first_frame_ms;
        transport.latency_method = report.latency_method;
        transport.latency_ms = report.latency_ms;
        transport.resets = report.resets;
        transport.stalled = report.stalled;
        transport.reported_unix_ms = Some(now);
    }
}

/// The rungs this build serves (ADR-0114 slice 2: HLS only).
pub const SERVED_RUNGS: &[&str] = &["hls"];

/// The rung to attach and the rest, best first (decision 3, "What the
/// engine leaves out", in that order): rungs the build or the run cannot
/// serve (`served`), rungs `[cast] ladder` does not enable (`enabled`, whose
/// order is the offer order), rungs the caps exclude (no presented-frame
/// counter: no `moq`, no `webrtc`), and rungs in the device's `skip`. `hls` is
/// the floor: it needs nothing a receiver claims and is never left out, so
/// the answer always attaches a rung.
#[must_use]
pub fn offer(
    enabled: &[String],
    served: &[&'static str],
    probe: Option<&ReceiverProbe>,
) -> (&'static str, Vec<&'static str>) {
    let no_frames = probe.is_none_or(|p| p.caps.frames.is_none());
    let skipped = |rung: &str| probe.is_some_and(|p| p.skip.iter().any(|s| s.rung == rung));
    let mut rungs: Vec<&'static str> = enabled
        .iter()
        .filter_map(|rung| served.iter().copied().find(|s| *s == rung))
        .filter(|rung| *rung == "hls" || !(no_frames && NEED_FRAME_COUNTER.contains(rung)))
        .filter(|rung| *rung == "hls" || !skipped(rung))
        .collect();
    rungs.dedup();
    if !rungs.contains(&"hls") {
        rungs.push("hls");
    }
    let attach = rungs.remove(0);
    (attach, rungs)
}

/// A validated ladder request.
#[derive(Debug, Clone, PartialEq)]
pub struct LadderRequest {
    /// The receiver's `t0` (µs since the Unix epoch, its monotonic clock).
    pub t0_us: Option<f64>,
    /// `false`: a clock-only call (`{"v":1,"clock":{"t0":…}}`).
    pub full: bool,
    /// The probe (on a full call).
    pub probe: Option<ReceiverProbe>,
    /// The rung that just failed, if any.
    pub failed: Option<Fallback>,
}

/// The `transport` part of a receiver report, validated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransportReport {
    pub(super) first_frame_ms: Option<u64>,
    pub(super) latency_method: Option<LatencyMethod>,
    pub(super) latency_ms: Option<LatencyStats>,
    pub(super) resets: Resets,
    pub(super) stalled: bool,
}

/// Why a ladder body was refused (the route answers `400`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub &'static str);

type Checked<T> = Result<T, Invalid>;

fn object<'a>(value: &'a Value, what: &'static str) -> Checked<&'a Map<String, Value>> {
    value.as_object().ok_or(Invalid(what))
}

/// A present, non-null field.
fn field<'a>(obj: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

fn opt_bool(obj: &Map<String, Value>, key: &'static str) -> Checked<Option<bool>> {
    field(obj, key)
        .map(|v| v.as_bool().ok_or(Invalid(key)))
        .transpose()
}

fn opt_str(obj: &Map<String, Value>, key: &'static str, max: usize) -> Checked<Option<String>> {
    field(obj, key)
        .map(|v| {
            let s = v.as_str().ok_or(Invalid(key))?;
            if s.chars().count() > max {
                return Err(Invalid(key));
            }
            Ok(s.to_owned())
        })
        .transpose()
}

fn opt_u64(obj: &Map<String, Value>, key: &'static str, max: u64) -> Checked<Option<u64>> {
    field(obj, key)
        .map(|v| {
            // An integer, or a float with no fraction (JSON from a browser):
            // anything else, negative or above `max`, is refused.
            let n = v.as_u64().or_else(|| {
                v.as_f64()
                    .filter(|f| f.is_finite() && *f >= 0.0 && f.fract() == 0.0 && *f <= 9.0e15)
                    .and_then(|f| format!("{f:.0}").parse().ok())
            });
            match n {
                Some(n) if n <= max => Ok(n),
                _ => Err(Invalid(key)),
            }
        })
        .transpose()
}

fn opt_support(obj: &Map<String, Value>, key: &'static str) -> Checked<Option<Support>> {
    opt_str(obj, key, MAX_SHORT)?
        .map(|s| match s.as_str() {
            "supported" => Ok(Support::Supported),
            "unsupported" => Ok(Support::Unsupported),
            "absent" => Ok(Support::Absent),
            _ => Err(Invalid(key)),
        })
        .transpose()
}

fn rung_name(s: &str, what: &'static str) -> Checked<String> {
    RUNGS
        .iter()
        .find(|r| **r == s)
        .map(|r| (*r).to_owned())
        .ok_or(Invalid(what))
}

fn caps(value: &Value) -> Checked<ReceiverCaps> {
    let obj = object(value, "caps")?;
    let rtc_h264 = match field(obj, "rtc_h264") {
        None => Vec::new(),
        Some(v) => {
            let list = v.as_array().ok_or(Invalid("rtc_h264"))?;
            if list.len() > MAX_H264_PROFILES {
                return Err(Invalid("rtc_h264"));
            }
            list.iter()
                .map(|p| {
                    let s = p.as_str().ok_or(Invalid("rtc_h264"))?;
                    if s.len() > MAX_SHORT || !s.chars().all(|c| c.is_ascii_alphanumeric()) {
                        return Err(Invalid("rtc_h264"));
                    }
                    Ok(s.to_owned())
                })
                .collect::<Checked<Vec<_>>>()?
        }
    };
    let frames = opt_str(obj, "frames", MAX_SHORT)?
        .map(|s| match s.as_str() {
            "rvfc" => Ok(FrameCounter::Rvfc),
            "quality" => Ok(FrameCounter::Quality),
            _ => Err(Invalid("frames")),
        })
        .transpose()?;
    Ok(ReceiverCaps {
        wt: opt_bool(obj, "wt")?,
        mse: opt_bool(obj, "mse")?,
        vdec: opt_support(obj, "vdec")?,
        vdec_hw: opt_bool(obj, "vdec_hw")?,
        adec: opt_support(obj, "adec")?,
        rtc: opt_bool(obj, "rtc")?,
        rtc_h264,
        rtc_opus: opt_bool(obj, "rtc_opus")?,
        rvfc: opt_bool(obj, "rvfc")?,
        frames,
        srt: opt_str(obj, "srt", MAX_SHORT)?,
    })
}

fn skip(value: &Value) -> Checked<Vec<SkipEntry>> {
    let obj = object(value, "skip")?;
    let mut out = Vec::new();
    // Unknown keys are ignored; a known rung must name a number of seconds.
    for rung in RUNGS {
        if let Some(seconds) = opt_u64(obj, rung, MAX_SKIP_SECONDS)? {
            if out.len() < MAX_SKIP {
                out.push(SkipEntry {
                    rung: (*rung).to_owned(),
                    seconds,
                });
            }
        }
    }
    Ok(out)
}

fn failed(value: &Value, now: u64) -> Checked<Fallback> {
    let obj = object(value, "failed")?;
    let rung = opt_str(obj, "rung", MAX_SHORT)?.ok_or(Invalid("failed.rung"))?;
    let rung = rung_name(&rung, "failed.rung")?;
    let stage = opt_str(obj, "stage", MAX_SHORT)?.ok_or(Invalid("failed.stage"))?;
    if !STAGES.contains(&stage.as_str()) {
        return Err(Invalid("failed.stage"));
    }
    Ok(Fallback {
        rung,
        stage,
        ms: opt_u64(obj, "ms", MAX_REPORTED_MS)?,
        at_unix_ms: now,
    })
}

/// Validate a ladder body (already at most [`MAX_LADDER_BYTES`]).
///
/// # Errors
/// [`Invalid`] naming the first field of a wrong type or out of bounds.
pub fn parse_request(body: &[u8], now: u64) -> Checked<LadderRequest> {
    let value: Value = serde_json::from_slice(body).map_err(|_| Invalid("json"))?;
    let obj = object(&value, "body")?;
    match obj.get("v").and_then(Value::as_u64) {
        Some(1) => {}
        _ => return Err(Invalid("v")),
    }
    let t0_us = match field(obj, "clock") {
        None => None,
        Some(clock) => {
            let clock = object(clock, "clock")?;
            field(clock, "t0")
                .map(|t0| {
                    let t0 = t0.as_f64().ok_or(Invalid("clock.t0"))?;
                    if !t0.is_finite() || !(0.0..=MAX_T0_US).contains(&t0) {
                        return Err(Invalid("clock.t0"));
                    }
                    Ok(t0)
                })
                .transpose()?
        }
    };
    let full = ["receiver", "caps", "need", "skip", "failed"]
        .iter()
        .any(|k| obj.contains_key(*k));
    if !full {
        return Ok(LadderRequest {
            t0_us,
            full: false,
            probe: None,
            failed: None,
        });
    }
    let (kind, build, ua) = match field(obj, "receiver") {
        None => (None, None, None),
        Some(receiver) => {
            let receiver = object(receiver, "receiver")?;
            (
                opt_str(receiver, "kind", MAX_SHORT)?,
                opt_str(receiver, "build", MAX_BUILD)?,
                opt_str(receiver, "ua", MAX_UA)?,
            )
        }
    };
    let caps = field(obj, "caps")
        .map(caps)
        .transpose()?
        .unwrap_or_default();
    let need = match field(obj, "need") {
        None => ReceiverNeed::default(),
        Some(need) => {
            let need = object(need, "need")?;
            ReceiverNeed {
                subtitles: opt_bool(need, "subtitles")?,
                audio: opt_str(need, "audio", MAX_SHORT)?,
            }
        }
    };
    let skip = field(obj, "skip")
        .map(skip)
        .transpose()?
        .unwrap_or_default();
    let failed = field(obj, "failed").map(|f| failed(f, now)).transpose()?;
    Ok(LadderRequest {
        t0_us,
        full: true,
        probe: Some(ReceiverProbe {
            kind,
            build,
            ua,
            caps,
            need,
            skip,
            received_unix_ms: now,
        }),
        failed,
    })
}

/// The `transport` part of a receiver report (lenient: `None` when absent
/// or malformed, so an older or broken receiver's report is still stored).
#[must_use]
pub fn parse_transport_report(report: &Value) -> Option<TransportReport> {
    let obj = report.get("transport")?.as_object()?;
    let method = opt_str(obj, "latency_method", MAX_SHORT)
        .ok()?
        .map(|s| match s.as_str() {
            "frame" => Some(LatencyMethod::Frame),
            "playhead" => Some(LatencyMethod::Playhead),
            "unavailable" => Some(LatencyMethod::Unavailable),
            _ => None,
        })
        .map_or(Some(None), |m| m.map(Some))?;
    let latency_ms = match field(obj, "latency_ms") {
        // A figure only with a method that allows one (decision 2).
        Some(latency) if matches!(method, Some(LatencyMethod::Frame | LatencyMethod::Playhead)) => {
            let latency = latency.as_object()?;
            let median = opt_u64(latency, "median", MAX_LATENCY_MS).ok()??;
            let p95 = opt_u64(latency, "p95", MAX_LATENCY_MS).ok()??;
            let samples = opt_u64(latency, "samples", 1_000_000).ok()??;
            Some(LatencyStats {
                median,
                p95: p95.max(median),
                samples,
                method: method?,
                clock_error_ms: opt_u64(latency, "clock_error_ms", MAX_LATENCY_MS).ok()?,
            })
        }
        _ => None,
    };
    let resets = match field(obj, "resets") {
        None => Resets::default(),
        Some(resets) => {
            let resets = resets.as_object()?;
            Resets {
                soft: opt_u64(resets, "soft", 1_000_000).ok()?.unwrap_or(0),
                hard: opt_u64(resets, "hard", 1_000_000).ok()?.unwrap_or(0),
                last: opt_str(resets, "last", MAX_SHORT).ok()?,
            }
        }
    };
    Some(TransportReport {
        first_frame_ms: opt_u64(obj, "first_frame_ms", MAX_REPORTED_MS).ok()?,
        latency_method: method,
        latency_ms,
        resets,
        stalled: opt_bool(obj, "stalled").ok()?.unwrap_or(false),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn parse(v: &Value) -> Checked<LadderRequest> {
        parse_request(serde_json::to_vec(v).unwrap().as_slice(), 7)
    }

    #[test]
    fn the_adr_example_parses_and_unknown_fields_are_ignored() {
        let req = parse(&serde_json::json!({
            "v": 1,
            "receiver": {"kind": "web", "build": "abc123", "ua": "Mozilla/5.0 CrKey", "extra": 1},
            "caps": {"wt": true, "mse": true, "vdec": "supported", "vdec_hw": true,
                     "adec": "supported", "rtc": true, "rtc_h264": ["42e01f", "640c1f"],
                     "rtc_opus": true, "rvfc": true, "frames": "rvfc", "srt": null, "future": [1]},
            "need": {"subtitles": false, "audio": null},
            "skip": {"moq": 540, "nonsense": "x"},
            "failed": null,
            "clock": {"t0": 1_794_705_012_345_000.5},
            "later": {"anything": true}
        }))
        .unwrap();
        assert!(req.full);
        assert_eq!(req.t0_us, Some(1_794_705_012_345_000.5));
        let probe = req.probe.unwrap();
        assert_eq!(probe.kind.as_deref(), Some("web"));
        assert_eq!(probe.caps.rtc_h264, ["42e01f", "640c1f"]);
        assert_eq!(probe.caps.frames, Some(FrameCounter::Rvfc));
        assert_eq!(probe.caps.vdec, Some(Support::Supported));
        assert_eq!(
            probe.skip,
            [SkipEntry {
                rung: "moq".into(),
                seconds: 540
            }]
        );
        assert_eq!(probe.received_unix_ms, 7);
    }

    #[test]
    fn clock_only_call() {
        let req = parse(&serde_json::json!({"v": 1, "clock": {"t0": 5}})).unwrap();
        assert!(!req.full);
        assert_eq!(req.t0_us, Some(5.0));
        assert!(req.probe.is_none());
    }

    #[test]
    fn wrong_types_and_bounds_are_refused() {
        let base = serde_json::json!({"v": 1, "caps": {}});
        let cases: Vec<(Value, &str)> = vec![
            (serde_json::json!([1]), "body"),
            (serde_json::json!({"v": 2}), "v"),
            (serde_json::json!({"v": "1"}), "v"),
            (serde_json::json!({}), "v"),
            (serde_json::json!({"v": 1, "clock": 3}), "clock"),
            (
                serde_json::json!({"v": 1, "clock": {"t0": "x"}}),
                "clock.t0",
            ),
            (serde_json::json!({"v": 1, "clock": {"t0": -1}}), "clock.t0"),
            (
                serde_json::json!({"v": 1, "clock": {"t0": 1e30}}),
                "clock.t0",
            ),
            (serde_json::json!({"v": 1, "caps": []}), "caps"),
            (serde_json::json!({"v": 1, "caps": {"wt": "yes"}}), "wt"),
            (
                serde_json::json!({"v": 1, "caps": {"vdec": "maybe"}}),
                "vdec",
            ),
            (
                serde_json::json!({"v": 1, "caps": {"frames": "other"}}),
                "frames",
            ),
            (
                serde_json::json!({"v": 1, "caps": {"rtc_h264": "42e01f"}}),
                "rtc_h264",
            ),
            (
                serde_json::json!({"v": 1, "caps": {"rtc_h264": ["a","b","c","d","e","f","g","h","i"]}}),
                "rtc_h264",
            ),
            (
                serde_json::json!({"v": 1, "caps": {"rtc_h264": ["42 e0"]}}),
                "rtc_h264",
            ),
            (
                serde_json::json!({"v": 1, "caps": {"srt": "x".repeat(33)}}),
                "srt",
            ),
            (
                serde_json::json!({"v": 1, "receiver": {"ua": "u".repeat(201)}}),
                "ua",
            ),
            (
                serde_json::json!({"v": 1, "receiver": {"build": 5}}),
                "build",
            ),
            (serde_json::json!({"v": 1, "receiver": "web"}), "receiver"),
            (
                serde_json::json!({"v": 1, "need": {"subtitles": 1}}),
                "subtitles",
            ),
            (serde_json::json!({"v": 1, "skip": {"moq": -5}}), "moq"),
            (serde_json::json!({"v": 1, "skip": {"moq": 100_000}}), "moq"),
            (serde_json::json!({"v": 1, "skip": []}), "skip"),
            (
                serde_json::json!({"v": 1, "failed": {"rung": "rtmp", "stage": "connect"}}),
                "failed.rung",
            ),
            (
                serde_json::json!({"v": 1, "failed": {"rung": "moq", "stage": "later"}}),
                "failed.stage",
            ),
            (
                serde_json::json!({"v": 1, "failed": {"rung": "moq", "stage": "stall", "ms": "x"}}),
                "ms",
            ),
        ];
        assert!(parse(&base).is_ok());
        for (body, field) in cases {
            assert_eq!(parse(&body).unwrap_err(), Invalid(field), "{body}");
        }
        assert_eq!(parse_request(b"{not json", 0).unwrap_err(), Invalid("json"));
    }

    #[test]
    fn the_hook_stage_is_accepted_and_every_stage_listed() {
        for stage in STAGES {
            let req =
                parse(&serde_json::json!({"v": 1, "failed": {"rung": "webrtc", "stage": stage}}))
                    .unwrap();
            assert_eq!(req.failed.unwrap().stage, *stage);
        }
        assert_eq!(
            STAGES,
            ["connect", "first-frame", "stall", "latency", "hook"]
        );
    }

    fn probe_with(frames: Option<FrameCounter>, skip: &[&str]) -> ReceiverProbe {
        ReceiverProbe {
            caps: ReceiverCaps {
                frames,
                ..ReceiverCaps::default()
            },
            skip: skip
                .iter()
                .map(|r| SkipEntry {
                    rung: (*r).to_owned(),
                    seconds: 60,
                })
                .collect(),
            ..ReceiverProbe::default()
        }
    }

    #[test]
    fn the_offer_of_this_build_is_hls_only() {
        let all: Vec<String> = ["srt", "moq", "webrtc", "hls"].map(String::from).to_vec();
        let probe = probe_with(Some(FrameCounter::Rvfc), &[]);
        assert_eq!(offer(&all, SERVED_RUNGS, Some(&probe)), ("hls", vec![]));
        assert_eq!(
            offer(&["hls".to_owned()], SERVED_RUNGS, None),
            ("hls", vec![])
        );
        assert_eq!(
            offer(&[], SERVED_RUNGS, None),
            ("hls", vec![]),
            "hls is the floor"
        );
    }

    /// The `frames` rule (decision 3, "Cast media status" rule 1), against
    /// a build that served every rung: without a presented-frame counter no
    /// `moq` and no `webrtc` is offered; `srt` and `hls` are.
    #[test]
    fn without_a_frame_counter_moq_and_webrtc_are_left_out() {
        let all: Vec<String> = ["srt", "moq", "webrtc", "hls"].map(String::from).to_vec();
        let every = &["srt", "moq", "webrtc", "hls"];
        for frames in [Some(FrameCounter::Rvfc), Some(FrameCounter::Quality)] {
            let probe = probe_with(frames, &[]);
            assert_eq!(
                offer(&all, every, Some(&probe)),
                ("srt", vec!["moq", "webrtc", "hls"]),
                "{frames:?}"
            );
        }
        let none = probe_with(None, &[]);
        assert_eq!(
            offer(&all, every, Some(&none)),
            ("srt", vec!["hls"]),
            "frames null"
        );
        assert_eq!(
            offer(&all, every, None),
            ("srt", vec!["hls"]),
            "no probe: no counter claimed"
        );
        // Absent from the body is the same as null.
        let req = parse(&serde_json::json!({"v": 1, "caps": {"wt": true}})).unwrap();
        assert_eq!(req.probe.as_ref().unwrap().caps.frames, None);
        assert_eq!(offer(&all, every, req.probe.as_ref()), ("srt", vec!["hls"]));
        // The device's skip list and the config's order apply too.
        let skip = probe_with(Some(FrameCounter::Rvfc), &["srt", "hls"]);
        let order: Vec<String> = ["webrtc", "moq", "hls"].map(String::from).to_vec();
        assert_eq!(
            offer(&order, every, Some(&skip)),
            ("webrtc", vec!["moq", "hls"])
        );
        assert_eq!(
            offer(&all, every, Some(&skip)),
            ("moq", vec!["webrtc", "hls"])
        );
    }

    #[test]
    fn failed_is_kept() {
        let req = parse(&serde_json::json!({"v": 1, "failed": {"rung": "moq", "stage": "first-frame", "ms": 3000}}))
            .unwrap();
        assert_eq!(
            req.failed,
            Some(Fallback {
                rung: "moq".into(),
                stage: "first-frame".into(),
                ms: Some(3000),
                at_unix_ms: 7
            })
        );
    }

    #[test]
    fn the_rate_is_one_per_second_with_a_burst_of_four() {
        let mut state = LadderState::default();
        let admitted = (0..10).filter(|_| state.admit(1000)).count();
        assert_eq!(admitted, 4);
        assert!(!state.admit(1999));
        assert!(state.admit(2000));
        assert!(!state.admit(2000));
        // A long pause refills to the burst, not beyond.
        assert_eq!((0..10).filter(|_| state.admit(60_000)).count(), 4);
    }

    #[test]
    fn fallbacks_are_bounded_and_the_rung_since_moves_only_on_change() {
        let mut state = LadderState::default();
        for i in 0..20 {
            state.offered(
                "hls",
                Some(Fallback {
                    rung: "moq".into(),
                    stage: "stall".into(),
                    ms: Some(i),
                    at_unix_ms: i,
                }),
                100 + i,
            );
        }
        let t = state.transport_view().unwrap();
        assert_eq!(t.fallbacks.len(), MAX_FALLBACKS);
        assert_eq!(t.fallbacks[0].ms, Some(12), "oldest forgotten first");
        assert_eq!(t.since_unix_ms, Some(100));
        assert_eq!(t.rung.as_deref(), Some("hls"));
    }

    #[test]
    fn transport_reports_are_lenient_and_never_invent_a_figure() {
        let ok = serde_json::json!({"transport": {
            "first_frame_ms": 412, "latency_method": "frame",
            "latency_ms": {"median": 7400, "p95": 7800, "samples": 140, "clock_error_ms": 3},
            "resets": {"soft": 1, "hard": 0, "last": "gap"}, "stalled": false}});
        let t = parse_transport_report(&ok).unwrap();
        assert_eq!(t.first_frame_ms, Some(412));
        assert_eq!(t.latency_ms.as_ref().unwrap().median, 7400);
        assert_eq!(t.latency_ms.unwrap().method, LatencyMethod::Frame);
        // `unavailable` carries no figure even when one is sent.
        let un = serde_json::json!({"transport": {"latency_method": "unavailable",
            "latency_ms": {"median": 1, "p95": 2, "samples": 3}}});
        let t = parse_transport_report(&un).unwrap();
        assert_eq!(t.latency_method, Some(LatencyMethod::Unavailable));
        assert!(t.latency_ms.is_none());
        // Malformed: ignored as a whole; absent: none.
        assert!(
            parse_transport_report(&serde_json::json!({"transport": {"stalled": "no"}})).is_none()
        );
        assert!(parse_transport_report(
            &serde_json::json!({"transport": {"latency_method": "frame",
            "latency_ms": {"median": 70_000, "p95": 1, "samples": 1}}})
        )
        .is_none());
        assert!(parse_transport_report(&serde_json::json!({"state": "playing"})).is_none());
    }
}
