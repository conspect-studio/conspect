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
}

impl Tools {
    /// Require trusted absolute, normalised paths to existing executables.
    /// No `PATH` lookup ever happens for a producer process.
    /// # Errors
    /// Rejects relative paths, `..` components, and non-executable files.
    pub fn validate(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for path in [&self.node, &self.browser, &self.ffmpeg] {
            let ok = path.is_absolute()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                && std::fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
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
    /// Output frame count, 2 through 600.
    pub frames: u32,
    /// Authored document. External network requests are blocked by the host.
    pub document: String,
    /// Optional Windy view; when present the authored document must be empty.
    #[serde(default)]
    pub windy: Option<WindyView>,
    /// Background mode; omitted means opaque (no alpha). Windy requires opaque.
    #[serde(default)]
    pub background: Background,
}

impl Capture {
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
