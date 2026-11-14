//! Actual background browser capture provider. No browser is started by default.
//! Only service-selected tools/templates enter this boundary; HTTP cannot submit HTML.
use serde::{Deserialize, Serialize};

/// Concurrent artifact streams allowed across all clients.
pub const ARTIFACT_STREAMS: usize = 2;

/// Windy terms version attached to this built-in template.
pub const WINDY_TERMS_VERSION: &str = "windy-2023-09-01";
/// One authorized, bounded public Windy view. Premium needs a configured secret resolver.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct CaptureBody {
    /// Fill authorization object.
    pub fill_id: String,
    /// Optional existing media player; validated by the route when supplied.
    /// Standalone artifact capture needs none; no player-load is implied.
    #[serde(default)]
    pub target_player: Option<String>,
    /// Site parameters, never a URL or executable path.
    pub view: View,
    /// User acceptance checked against the current template version.
    pub terms: Terms,
}
/// Site-specific bounded view.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct View {
    /// Latitude.
    pub lat: f64,
    /// Longitude.
    pub lon: f64,
    /// Map zoom.
    pub zoom: u8,
    /// Radar or wind.
    pub overlay: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Capture cadence.
    pub fps: u32,
    /// Capture frame count.
    pub frames: u32,
    /// Explicitly use the public unauthenticated page. Default requests Premium.
    #[serde(default)]
    pub public: bool,
    /// `steps` iterates the radar timeline (default for radar; radar only);
    /// `realtime` records `frames` wall-clock screenshots (default for wind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureMode>,
    /// Steps-mode per-step duration rule (default `equal`).
    #[serde(default)]
    pub timing: StepTiming,
    /// Steps-mode per-step duration in ms, 100 through 5000 (default 600).
    #[serde(default = "default_frame_ms")]
    pub frame_ms: u32,
    /// Steps-mode hold of the newest step in ms, 0 through 10000 (default 1000).
    #[serde(default = "default_end_hold_ms")]
    pub end_hold_ms: u32,
    /// Steps-mode count of most recent radar steps, 2 through 48 (default 12).
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}
const fn default_frame_ms() -> u32 {
    multiview_webengine::capture::DEFAULT_FRAME_MS
}
const fn default_end_hold_ms() -> u32 {
    multiview_webengine::capture::DEFAULT_END_HOLD_MS
}
const fn default_max_steps() -> u32 {
    multiview_webengine::capture::DEFAULT_MAX_STEPS
}
/// How the page animation becomes frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// One screenshot per radar timestamp; identical consecutive steps dropped.
    Steps,
    /// Wall-clock screenshots `1000/fps` ms apart.
    Realtime,
}
/// Steps-mode duration rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum StepTiming {
    /// Every step lasts `frame_ms`.
    #[default]
    Equal,
    /// Proportional to the real gap between radar timestamps, mean `frame_ms`,
    /// clamped to `[frame_ms/4, frame_ms*4]`.
    Proportional,
}
impl View {
    /// The engine capture options this view requests.
    #[must_use]
    pub fn capture_options(&self) -> multiview_webengine::capture::CaptureOptions {
        use multiview_webengine::capture as c;
        c::CaptureOptions {
            mode: self.capture.map(|m| match m {
                CaptureMode::Steps => c::CaptureMode::Steps,
                CaptureMode::Realtime => c::CaptureMode::Realtime,
            }),
            timing: match self.timing {
                StepTiming::Equal => c::StepTiming::Equal,
                StepTiming::Proportional => c::StepTiming::Proportional,
            },
            frame_ms: self.frame_ms,
            end_hold_ms: self.end_hold_ms,
            max_steps: self.max_steps,
        }
    }
}
/// Explicit versioned site-terms acceptance.
#[derive(Deserialize, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct Terms {
    /// Must be true.
    pub accepted: bool,
    /// Must match the built-in template version.
    pub version: String,
}
/// Public job status projection with no URL, credentials, document or disk path.
#[derive(Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobView {
    /// Stable opaque job number.
    pub job_id: u64,
    /// Authorized fill identity.
    pub fill_id: String,
    /// Queued, running, succeeded, failed, cancelled or uncertain.
    pub state: &'static str,
    /// Present only after the immutable artifact is committed.
    pub artifact_id: Option<String>,
    /// Estimated producer resources; GPU remains unknown.
    pub resources: Option<serde_json::Value>,
}
/// A fresh or replayed admission.
pub struct Admission {
    /// Job number.
    pub job_id: u64,
    /// True only for the first use of the idempotency key (audit once).
    pub fresh: bool,
}
/// Cancel result mapped by the route.
#[derive(Debug, PartialEq, Eq)]
pub struct CancelResult {
    /// True only for the first accepted use of the key.
    pub fresh: bool,
}
#[cfg(unix)]
mod provider;
#[cfg(unix)]
pub use provider::Provider;
#[cfg(not(unix))]
mod provider_unavailable;
#[cfg(not(unix))]
pub use provider_unavailable::{ArtifactLease, Provider};
