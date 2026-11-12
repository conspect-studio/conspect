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
