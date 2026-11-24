//! ISO + Program recording config (ADR-0037 / REC-8).
//!
//! Additive and serde-default: a document that never mentions recording parses
//! and re-serialises exactly as before. Every union is internally tagged by
//! `kind` (never `untagged`, ADR-0010).
//!
//! `program` is an optional field on every [`crate::Output`] variant.
//! An absent block leaves recording off; the field is serde-default and
//! omitted on serialization so existing documents remain unchanged.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::schema::SourceKind;

/// How long one recording segment stays open before the writer rotates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SegmentPolicy {
    /// Target segment length in whole seconds. Must be `> 0`.
    ///
    /// Rotation is still keyframe-aligned (ADR-0032): this is the target, not
    /// a hard cut inside a GOP.
    pub duration_seconds: u64,
}

impl SegmentPolicy {
    /// A segment target of `seconds`.
    #[must_use]
    pub const fn new(seconds: u64) -> Self {
        Self {
            duration_seconds: seconds,
        }
    }

    /// The target as a [`Duration`].
    #[must_use]
    pub const fn duration(self) -> Duration {
        Duration::from_secs(self.duration_seconds)
    }
}

/// Dual-axis retention (ADR-0037 decision 4). At least one bound is required
/// when the recorder is armed; an unarmed block may omit both (it writes
/// nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct RetentionPolicy {
    /// Drop a closed segment once it is older than this many whole seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_duration_seconds: Option<u64>,
    /// Drop the oldest closed segment once the location exceeds this many
    /// gibibytes (`1024^3` bytes, matching `df`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_size_gib: Option<u64>,
    /// Hard cap on the number of closed segments kept. Absent means "no count
    /// cap" — time or size still applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_segments: Option<u32>,
}

impl RetentionPolicy {
    /// Whether at least one prune axis is set.
    #[must_use]
    pub const fn has_bound(self) -> bool {
        self.keep_duration_seconds.is_some()
            || self.keep_size_gib.is_some()
            || self.max_segments.is_some()
    }

    /// The time bound, if any.
    #[must_use]
    pub const fn keep_duration(self) -> Option<Duration> {
        match self.keep_duration_seconds {
            Some(seconds) => Some(Duration::from_secs(seconds)),
            None => None,
        }
    }

    /// The size bound in bytes, if any.
    #[must_use]
    pub const fn keep_size_bytes(self) -> Option<u64> {
        match self.keep_size_gib {
            Some(gib) => Some(gib.saturating_mul(1024 * 1024 * 1024)),
            None => None,
        }
    }
}

/// When a recorder stops writing because the volume is too full, and when it
/// may start again (ADR-0037 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DiskPolicy {
    /// Stop when fewer than this many gibibytes are available to an
    /// unprivileged writer (`statvfs` `f_bavail`, the `df` number).
    pub min_free_gib: u64,
    /// Stop when used percent of the volume is at least this. `1..=99`.
    pub stop_at_pct: u8,
    /// Resume only after used percent has fallen this far below
    /// [`stop_at_pct`](Self::stop_at_pct). Default 5. Stops a volume that
    /// hovers on the threshold from arming and disarming every segment.
    #[serde(default = "default_hysteresis_pct")]
    pub resume_hysteresis_pct: u8,
}

/// The default resume band: five percentage points under the stop threshold.
const fn default_hysteresis_pct() -> u8 {
    5
}

impl DiskPolicy {
    /// A policy that stops at `stop_at_pct` percent used or below
    /// `min_free_gib`, and resumes five points under the stop threshold.
    #[must_use]
    pub const fn new(min_free_gib: u64, stop_at_pct: u8) -> Self {
        Self {
            min_free_gib,
            stop_at_pct,
            resume_hysteresis_pct: default_hysteresis_pct(),
        }
    }

    /// Used-percent at or above which a recording pauses.
    #[must_use]
    pub const fn stop_pct(self) -> u8 {
        self.stop_at_pct
    }

    /// Used-percent at or below which a paused recording may resume.
    ///
    /// Saturates at 1 so a `stop_at_pct` of 1 with a wide hysteresis still has
    /// a resume point the sensor can observe.
    #[must_use]
    pub const fn resume_pct(self) -> u8 {
        let band = self.resume_hysteresis_pct;
        let stop = self.stop_at_pct;
        if band >= stop {
            1
        } else {
            stop - band
        }
    }

    /// Minimum free bytes (`f_bavail`), saturating.
    #[must_use]
    pub const fn min_free_bytes(self) -> u64 {
        self.min_free_gib.saturating_mul(1024 * 1024 * 1024)
    }
}

/// What an ISO recorder writes (ADR-0037 decision 3).
///
/// `ByteExact` is parsed so a document can name it, and
/// [`validate_iso`](self::validate_iso) rejects it on every transport that is
/// not a byte stream. v1 has no raw-AVIO tee, so even a legal `ByteExact`
/// stays a validated-but-unwired future (the backlog's deferred list).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum IsoContainer {
    /// Re-container every elementary stream without re-encoding.
    Remux {
        /// `mkv` (the ISO default; carries arbitrary streams) or `ts`.
        format: RemuxFormat,
    },
    /// Byte-for-byte on-wire passthrough. Legal only on `Ts` and `Srt`
    /// sources, and not implemented in v1.
    ByteExact,
}

impl Default for IsoContainer {
    fn default() -> Self {
        Self::Remux {
            format: RemuxFormat::Mkv,
        }
    }
}

/// The container a copy-remux ISO writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RemuxFormat {
    /// Matroska. Carries every elementary stream the demuxer reports.
    Mkv,
    /// MPEG-TS. Carries the streams a TS muxer can represent.
    Ts,
}

/// Which program picture a Program recorder captures (ADR-0037 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProgramFeed {
    /// The composited program after graphics are baked. v1 records this and
    /// only this — it is the encode that already exists (invariant #7).
    #[default]
    Dirty,
    /// The overlay-free program. A future separate encode under its own
    /// rendition id; validation accepts the token and the sink refuses to arm
    /// it until that encode exists.
    Clean,
}

/// An isolated per-source recording (ISO).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct IsoRecording {
    /// Whether the recorder starts armed. Default `false`: declaring an ISO
    /// never starts writing on its own.
    #[serde(default)]
    pub armed: bool,
    /// Directory the segments land in. Absent ⇒ the engine's recording root
    /// joined with the source id. Must be absolute when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// How long a segment stays open.
    pub segment: SegmentPolicy,
    /// What to keep.
    pub retention: RetentionPolicy,
    /// When the volume is too full to keep writing.
    pub disk: DiskPolicy,
    /// What to write. Absent ⇒ copy-remux into Matroska.
    #[serde(default, skip_serializing_if = "IsoContainer::is_default")]
    pub container: IsoContainer,
}

impl IsoContainer {
    /// Whether this is the default container (so it can be omitted on write).
    #[must_use]
    pub fn is_default(&self) -> bool {
        matches!(
            self,
            Self::Remux {
                format: RemuxFormat::Mkv
            }
        )
    }
}

impl IsoRecording {
    /// The container, defaulting a missing one to Matroska remux.
    #[must_use]
    pub fn container(&self) -> &IsoContainer {
        &self.container
    }
}

/// A recording of one output's composited program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProgramRecording {
    /// Whether the recorder starts armed. Default `false`.
    #[serde(default)]
    pub armed: bool,
    /// Directory the segments land in. Absent ⇒ the engine's recording root
    /// joined with the output id. Must be absolute when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// How long a segment stays open.
    pub segment: SegmentPolicy,
    /// What to keep.
    pub retention: RetentionPolicy,
    /// When the volume is too full to keep writing.
    pub disk: DiskPolicy,
    /// Which program picture to capture. Absent ⇒ the dirty program.
    #[serde(default, skip_serializing_if = "ProgramFeed::is_default_ref")]
    pub feed: ProgramFeed,
}

impl ProgramFeed {
    /// Whether this is the v1 default (so it can be omitted on write).
    #[must_use]
    pub const fn is_default(self) -> bool {
        matches!(self, Self::Dirty)
    }

    /// The serde skip predicate: it is handed a reference.
    #[must_use]
    pub const fn is_default_ref(feed: &Self) -> bool {
        feed.is_default()
    }
}

/// Reject an ISO block that cannot be honoured.
///
/// `owner` is the source id, used only in the error text.
///
/// # Errors
///
/// Returns [`ConfigError::Validation`] naming the first broken rule.
pub fn validate_iso(
    owner: &str,
    recording: &IsoRecording,
    kind: &SourceKind,
) -> Result<(), ConfigError> {
    validate_common(
        &format!("source {owner:?} iso"),
        recording.armed,
        recording.location.as_deref(),
        recording.segment,
        &recording.retention,
        &recording.disk,
    )?;
    if matches!(recording.container, IsoContainer::ByteExact) && !byte_exact_legal(kind) {
        return Err(ConfigError::Validation(format!(
            "source {owner:?} iso.container = byte_exact is only legal on a ts or srt source \
             (the on-wire bytes of rtmp, hls and rtsp are not recoverable); use remux"
        )));
    }
    Ok(())
}

/// Reject a Program recording block that cannot be honoured.
///
/// # Errors
///
/// Returns [`ConfigError::Validation`] naming the first broken rule.
pub fn validate_program(owner: &str, recording: &ProgramRecording) -> Result<(), ConfigError> {
    validate_common(
        &format!("output {owner:?} program"),
        recording.armed,
        recording.location.as_deref(),
        recording.segment,
        &recording.retention,
        &recording.disk,
    )
}

/// `ByteExact` is only meaningful where the on-wire bytes are the elementary
/// stream: an MPEG-TS input or an SRT input (which carries MPEG-TS).
fn byte_exact_legal(kind: &SourceKind) -> bool {
    matches!(kind, SourceKind::Ts { .. } | SourceKind::Srt { .. })
}

fn validate_common(
    what: &str,
    armed: bool,
    location: Option<&str>,
    segment: SegmentPolicy,
    retention: &RetentionPolicy,
    disk: &DiskPolicy,
) -> Result<(), ConfigError> {
    if segment.duration_seconds == 0 {
        return Err(ConfigError::Validation(format!(
            "{what}: segment.duration_seconds must be > 0"
        )));
    }
    if !(1..=99).contains(&disk.stop_at_pct) {
        return Err(ConfigError::Validation(format!(
            "{what}: disk.stop_at_pct must be 1..=99 (got {})",
            disk.stop_at_pct
        )));
    }
    if disk.resume_hysteresis_pct == 0 || disk.resume_hysteresis_pct > 99 {
        return Err(ConfigError::Validation(format!(
            "{what}: disk.resume_hysteresis_pct must be 1..=99 (got {})",
            disk.resume_hysteresis_pct
        )));
    }
    if armed && !retention.has_bound() {
        return Err(ConfigError::Validation(format!(
            "{what}: an armed recording needs at least one retention bound \
             (keep_duration_seconds, keep_size_gib, or max_segments)"
        )));
    }
    if retention.keep_duration_seconds == Some(0)
        || retention.keep_size_gib == Some(0)
        || retention.max_segments == Some(0)
        || disk.min_free_gib == 0
    {
        return Err(ConfigError::Validation(format!(
            "{what}: retention bounds and disk.min_free_gib must be positive"
        )));
    }
    if let Some(path) = location {
        if path.is_empty() || !path.starts_with('/') {
            return Err(ConfigError::Validation(format!(
                "{what}: location must be an absolute path (got {path:?})"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk() -> DiskPolicy {
        DiskPolicy::new(10, 80)
    }

    fn iso(armed: bool) -> IsoRecording {
        IsoRecording {
            armed,
            location: Some("/var/lib/conspect/rec/cam1".to_owned()),
            segment: SegmentPolicy::new(6),
            retention: RetentionPolicy {
                keep_duration_seconds: Some(3600),
                keep_size_gib: None,
                max_segments: None,
            },
            disk: disk(),
            container: IsoContainer::default(),
        }
    }

    #[test]
    fn an_armed_iso_with_one_bound_validates() {
        let kind = SourceKind::Srt {
            url: "srt://[::1]:9000".to_owned(),
            srt: None,
        };
        validate_iso("cam1", &iso(true), &kind).unwrap();
    }

    #[test]
    fn an_armed_recording_without_a_retention_bound_is_rejected() {
        let mut recording = iso(true);
        recording.retention = RetentionPolicy::default();
        let err = validate_iso("cam1", &recording, &SourceKind::Bars).unwrap_err();
        assert!(
            err.to_string().contains("at least one retention bound"),
            "{err}"
        );
    }

    #[test]
    fn a_zero_segment_duration_is_rejected() {
        let mut recording = iso(false);
        recording.segment = SegmentPolicy::new(0);
        let err = validate_iso("cam1", &recording, &SourceKind::Bars).unwrap_err();
        assert!(err.to_string().contains("duration_seconds"), "{err}");
    }

    #[test]
    fn zero_retention_or_disk_budget_is_rejected() {
        let kind = SourceKind::Bars;
        let mut recording = iso(true);
        recording.retention.keep_duration_seconds = Some(0);
        assert!(validate_iso("cam1", &recording, &kind).is_err());
        recording.retention.keep_duration_seconds = Some(3600);
        recording.retention.keep_size_gib = Some(0);
        assert!(validate_iso("cam1", &recording, &kind).is_err());
        recording.retention.keep_size_gib = None;
        recording.retention.max_segments = Some(0);
        assert!(validate_iso("cam1", &recording, &kind).is_err());
        recording.retention.max_segments = None;
        recording.disk.min_free_gib = 0;
        assert!(validate_iso("cam1", &recording, &kind).is_err());
    }

    #[test]
    fn stop_at_pct_outside_1_to_99_is_rejected() {
        for pct in [0_u8, 100] {
            let mut recording = iso(false);
            recording.disk.stop_at_pct = pct;
            let err = validate_iso("cam1", &recording, &SourceKind::Bars).unwrap_err();
            assert!(err.to_string().contains("stop_at_pct"), "{err}");
        }
    }

    #[test]
    fn low_stop_percent_resumes_at_saturated_one_percent() {
        let mut recording = iso(false);
        recording.disk.stop_at_pct = 1;
        validate_iso("cam1", &recording, &SourceKind::Bars).unwrap();
        assert_eq!(recording.disk.resume_pct(), 1);
    }

    #[test]
    fn byte_exact_is_rejected_on_rtmp_hls_and_rtsp() {
        let mut recording = iso(false);
        recording.container = IsoContainer::ByteExact;
        for kind in [
            SourceKind::Rtmp {
                url: "rtmp://example/live".to_owned(),
            },
            SourceKind::Hls {
                url: "https://example/live.m3u8".to_owned(),
                user_agent: None,
                referrer: None,
            },
            SourceKind::Rtsp {
                url: "rtsp://example/live".to_owned(),
            },
        ] {
            let err = validate_iso("cam1", &recording, &kind).unwrap_err();
            assert!(err.to_string().contains("byte_exact"), "{kind:?}: {err}");
        }
    }

    #[test]
    fn byte_exact_is_accepted_on_ts_and_srt() {
        let mut recording = iso(false);
        recording.container = IsoContainer::ByteExact;
        for kind in [
            SourceKind::Ts {
                url: "udp://[ff3e::1]:5000".to_owned(),
            },
            SourceKind::Srt {
                url: "srt://[::1]:9000".to_owned(),
                srt: None,
            },
        ] {
            validate_iso("cam1", &recording, &kind).unwrap();
        }
    }

    #[test]
    fn toml_and_json_round_trip() {
        let mut recording = iso(true);
        let toml_text = toml::to_string(&recording).unwrap();
        let back: IsoRecording = toml::from_str(&toml_text).unwrap();
        assert_eq!(back, recording);
        let json = serde_json::to_string(&recording).unwrap();
        let back: IsoRecording = serde_json::from_str(&json).unwrap();
        assert_eq!(back, recording);
        // The default container is omitted, so an existing document that
        // never names one stays byte-stable and still reads back as Matroska.
        assert!(!json.contains("\"container\""), "{json}");
        recording.container = IsoContainer::ByteExact;
        let explicit = serde_json::to_string(&recording).unwrap();
        assert!(explicit.contains("\"kind\":\"byte_exact\""), "{explicit}");
    }

    #[test]
    fn an_absent_container_deserialises_as_mkv_remux() {
        let text = r#"
            armed = false
            location = "/var/lib/conspect/rec/cam1"
            segment = { duration_seconds = 6 }
            retention = { keep_duration_seconds = 60 }
            disk = { min_free_gib = 10, stop_at_pct = 80 }
        "#;
        let recording: IsoRecording = toml::from_str(text).unwrap();
        assert_eq!(recording.container, IsoContainer::default());
        assert_eq!(recording.disk.resume_hysteresis_pct, 5);
    }
}
