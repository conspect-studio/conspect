//! Browser-capture request contract. Credentials never appear in command arguments.
use serde::{Deserialize, Serialize};

/// Trusted tools selected by the service, never by an API request.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tools {
    /// Node executable.
    pub node: std::path::PathBuf,
    /// Chromium headless-shell executable.
    pub browser: std::path::PathBuf,
    /// LGPL-compatible `FFmpeg` executable.
    pub ffmpeg: std::path::PathBuf,
    /// Browser GL backend chosen by the deployment (`CONSPECT_WEB_BROWSER_GL`),
    /// never by an API request. Omitted means the browser default (no flags).
    #[serde(default)]
    pub gl: BrowserGl,
}

/// Deployment-only browser GL mode. No mode ever adds `--disable-gpu`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserGl {
    /// No GL flags: the browser's own selection.
    #[default]
    Default,
    /// Software WebGL for GPU-less hosts:
    /// `--use-angle=swiftshader --enable-unsafe-swiftshader`.
    Swiftshader,
    /// Real GPU through EGL: `--use-gl=angle --use-angle=gl-egl`.
    Egl,
}

impl std::str::FromStr for BrowserGl {
    type Err = std::io::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "default" => Ok(Self::Default),
            "swiftshader" => Ok(Self::Swiftshader),
            "egl" => Ok(Self::Egl),
            _ => Err(std::io::Error::other("invalid browser GL mode")),
        }
    }
}

/// How the page animation becomes frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Drive the page's radar timeline (`W.store` `radarCalendar`): one
    /// screenshot per step, identical consecutive steps dropped. Default for
    /// Windy radar.
    Steps,
    /// Wall-clock screenshots `1000/fps` ms apart (`frames` of them). Default
    /// for Windy wind (particles have no step timeline) and authored HTML.
    Realtime,
}

/// Per-step duration rule for [`CaptureMode::Steps`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepTiming {
    /// Every step lasts `frame_ms`.
    #[default]
    Equal,
    /// Step duration follows the real gap to the next radar timestamp,
    /// normalised to a mean of `frame_ms`, clamped to `[frame_ms/4, frame_ms*4]`.
    Proportional,
}

/// Default per-step duration in milliseconds.
pub const DEFAULT_FRAME_MS: u32 = 600;
/// Default hold of the newest step before the loop wraps, in milliseconds.
pub const DEFAULT_END_HOLD_MS: u32 = 1000;
/// Default number of most recent radar steps captured.
pub const DEFAULT_MAX_STEPS: u32 = 12;
/// Hard cap on encoded output frames.
pub const MAX_FRAMES: u32 = 600;

const fn default_frame_ms() -> u32 {
    DEFAULT_FRAME_MS
}
const fn default_end_hold_ms() -> u32 {
    DEFAULT_END_HOLD_MS
}
const fn default_max_steps() -> u32 {
    DEFAULT_MAX_STEPS
}

/// Frames for a duration at `fps`: `round(ms * fps / 1000)`, integer math only.
#[must_use]
pub fn frames_for_ms(ms: u32, fps: u32) -> u64 {
    (u64::from(ms) * u64::from(fps) + 500) / 1000
}

impl Tools {
    /// Require trusted absolute, normalised paths to existing executables.
    /// No `PATH` lookup ever happens for a producer process.
    /// # Errors
    /// Rejects relative paths, `..` components, and non-executable files.
    pub fn validate(&self) -> std::io::Result<()> {
        for path in [&self.node, &self.browser, &self.ffmpeg] {
            let ok = path.is_absolute()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                && std::fs::metadata(path).is_ok_and(|m| m.is_file() && is_executable(&m));
            if !ok {
                return Err(std::io::Error::other("untrusted or missing tool path"));
            }
        }
        Ok(())
    }
}

/// Page background handling for authored HTML captures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    /// Straight-alpha capture (opt-in): the browser default background is fully
    /// transparent and the MOV is validated to carry a real alpha plane.
    Transparent,
    /// Browser default (opaque white) background; no alpha in the output.
    #[default]
    Opaque,
}

/// Bounded capture of a service-authored HTML document.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    /// Pixel width, even, 64 through 3840.
    pub width: u32,
    /// Pixel height, even, 64 through 2160.
    pub height: u32,
    /// Output frames per second, 1 through 30.
    pub fps: u32,
    /// Output frame count, 2 through 600, for [`CaptureMode::Realtime`]. In
    /// steps mode the producer derives the actual count from the kept steps.
    pub frames: u32,
    /// Authored document. External network requests are blocked by the host.
    pub document: String,
    /// Optional Windy view; when present the authored document must be empty.
    #[serde(default)]
    pub windy: Option<WindyView>,
    /// Background mode; omitted means opaque (no alpha). Windy requires opaque.
    #[serde(default)]
    pub background: Background,
    /// Capture mode and step-loop options; omitted means defaults.
    #[serde(default)]
    pub capture: CaptureOptions,
}

/// Capture mode plus the step-loop knobs (ignored in realtime mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureOptions {
    /// Capture mode; omitted means steps for Windy radar, realtime otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CaptureMode>,
    /// Steps-mode duration rule (default equal).
    #[serde(default)]
    pub timing: StepTiming,
    /// Steps-mode per-step duration, 100 through 5000 ms (default 600).
    #[serde(default = "default_frame_ms")]
    pub frame_ms: u32,
    /// Steps-mode hold on the newest step, 0 through 10000 ms (default 1000).
    #[serde(default = "default_end_hold_ms")]
    pub end_hold_ms: u32,
    /// Steps-mode count of most recent radar steps, 2 through 48 (default 12).
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            mode: None,
            timing: StepTiming::Equal,
            frame_ms: DEFAULT_FRAME_MS,
            end_hold_ms: DEFAULT_END_HOLD_MS,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }
}

impl Capture {
    /// The mode actually used: explicit, else steps for Windy radar, else realtime.
    #[must_use]
    pub fn effective_capture(&self) -> CaptureMode {
        self.capture.mode.unwrap_or_else(|| match &self.windy {
            Some(view) if view.overlay == "radar" => CaptureMode::Steps,
            _ => CaptureMode::Realtime,
        })
    }

    /// Validate allocation and runtime bounds before starting processes.
    /// # Errors
    /// Rejects unsupported geometry, count, cadence or document size.
    pub fn validate(&self) -> std::io::Result<()> {
        if !(64..=3840).contains(&self.width)
            || !(64..=2160).contains(&self.height)
            || self.width % 2 != 0
            || self.height % 2 != 0
            || !(1..=30).contains(&self.fps)
            || !(2..=600).contains(&self.frames)
            || self.document.len() > 128 * 1024
        {
            return Err(std::io::Error::other("invalid capture limits"));
        }
        if let Some(view) = &self.windy {
            if !self.document.is_empty()
                || self.width < 1024
                || self.background != Background::Opaque
            {
                return Err(std::io::Error::other(
                    "Windy requires desktop geometry and no document",
                ));
            }
            view.validate()?;
        }
        if self.effective_capture() == CaptureMode::Steps {
            // Steps drive a radar timeline: Windy radar, or an authored page
            // exposing the same `W.store` contract. Wind has no step timeline.
            let o = &self.capture;
            let nominal = u64::from(o.max_steps) * frames_for_ms(o.frame_ms, self.fps).max(1)
                + frames_for_ms(o.end_hold_ms, self.fps);
            if self.windy.as_ref().is_some_and(|v| v.overlay != "radar")
                || self.background != Background::Opaque
                || !(100..=5000).contains(&o.frame_ms)
                || o.end_hold_ms > 10_000
                || !(2..=48).contains(&o.max_steps)
                || nominal > u64::from(MAX_FRAMES)
            {
                return Err(std::io::Error::other("invalid step capture limits"));
            }
        }
        Ok(())
    }
}

/// A bounded, versioned Windy view. Authentication is not silently optional.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindyView {
    /// Latitude in degrees.
    pub lat: f64,
    /// Longitude in degrees.
    pub lon: f64,
    /// Integer map zoom.
    pub zoom: u8,
    /// Requested layer: radar or wind.
    pub overlay: String,
    /// Explicit acceptance of the site's terms, recorded by the caller.
    pub terms_accepted: bool,
    /// Require an authenticated Premium session; fail closed if absent.
    pub require_premium: bool,
    /// Ephemeral login material supplied by the trusted secret resolver.
    /// Never persist this request or include it in diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<WindyCredentials>,
}

/// Secret input carried only through the producer's private stdin pipe.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WindyCredentials {
    /// Login name resolved by the service.
    pub username: String,
    /// Password resolved by the service.
    pub password: String,
}
impl WindyView {
    /// Validate site parameters and explicit terms acceptance.
    /// # Errors
    /// Rejects invalid coordinates, zoom, layer or missing acceptance.
    pub fn validate(&self) -> std::io::Result<()> {
        if !self.lat.is_finite()
            || !(-85.0..=85.0).contains(&self.lat)
            || !self.lon.is_finite()
            || !(-180.0..=180.0).contains(&self.lon)
            || !(1..=18).contains(&self.zoom)
            || !matches!(self.overlay.as_str(), "radar" | "wind")
            || !self.terms_accepted
        {
            return Err(std::io::Error::other(
                "invalid Windy view or terms acceptance",
            ));
        }
        if let Some(credentials) = &self.credentials {
            if credentials.username.is_empty()
                || credentials.username.len() > 256
                || credentials.password.is_empty()
                || credentials.password.len() > 1024
                || credentials.username.chars().any(char::is_control)
                || credentials.password.contains('\0')
            {
                return Err(std::io::Error::other("invalid Windy credential input"));
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn is_executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

/// No executable bit off unix; the capture runtime does not run there.
#[cfg(not(unix))]
fn is_executable(_m: &std::fs::Metadata) -> bool {
    false
}
