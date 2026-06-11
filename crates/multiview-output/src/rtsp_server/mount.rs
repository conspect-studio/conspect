//! The RTSP **mount point** path + served-URL construction.
//!
//! `gst-rtsp-server` exposes each stream at a rooted mount path
//! (`/program`, `/multiview/cam1`, …); a client connects to
//! `rtsp://host:port/<mount>`. [`RtspMount`] validates and normalizes the mount
//! once (rooted, no surrounding-slash ambiguity, no whitespace/control
//! characters) so the path handed to the server and the URL surfaced to operators
//! are always well-formed — checked formatting only, no panics, no indexing, no
//! `as` casts (guardrails).
//!
//! This is a pure-Rust, always-compiled building block — no `GStreamer` dependency,
//! fully CI-testable without the `rtsp-server` feature.

use thiserror::Error;

/// A validated, normalized RTSP mount point.
///
/// Constructed from an operator-supplied mount string; stores the **rooted**
/// path (exactly one leading `/`, no trailing `/`) and builds the served URL on
/// demand from a host + port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspMount {
    /// The rooted mount path, e.g. `/program` (always one leading slash, no
    /// trailing slash, no surrounding-slash ambiguity).
    path: String,
}

impl RtspMount {
    /// Validate and normalize `mount` into a rooted RTSP mount path.
    ///
    /// Surrounding slashes are collapsed so `program`, `/program`, and
    /// `//program//` all normalize to `/program`; interior path segments are
    /// preserved. The result is always exactly one leading `/` and no trailing
    /// `/`.
    ///
    /// # Errors
    ///
    /// Returns [`RtspMountError::Empty`] when the mount is empty or only slashes,
    /// and [`RtspMountError::InvalidPath`] when it contains whitespace or control
    /// characters (which would produce an invalid RTSP URL / mount path).
    pub fn new(mount: impl AsRef<str>) -> Result<Self, RtspMountError> {
        let raw = mount.as_ref();
        let trimmed = raw.trim_matches('/');
        if trimmed.is_empty() {
            return Err(RtspMountError::Empty);
        }
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(RtspMountError::InvalidPath {
                mount: raw.to_owned(),
            });
        }
        Ok(Self {
            path: format!("/{trimmed}"),
        })
    }

    /// The rooted mount path (`/program`) handed to the `gst-rtsp-server`
    /// mount-point factory.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The full RTSP URL a client connects to, given the serving `host` and
    /// `port`: `rtsp://host:port/mount`.
    ///
    /// Pure checked formatting — never panics, never indexes.
    #[must_use]
    pub fn served_url(&self, host: &str, port: u16) -> String {
        // `self.path` already starts with exactly one `/`.
        format!("rtsp://{host}:{port}{}", self.path)
    }
}

/// Why an [`RtspMount`] could not be constructed.
///
/// `#[non_exhaustive]`: downstream `match` must carry a wildcard so new
/// validation arms are additive.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspMountError {
    /// The mount was empty or contained only slashes — there is no path to serve.
    #[error("rtsp mount is empty")]
    Empty,

    /// The mount contained whitespace or control characters, which would produce
    /// an invalid RTSP URL / mount path.
    #[error("rtsp mount `{mount}` contains whitespace or control characters")]
    InvalidPath {
        /// The offending mount string.
        mount: String,
    },
}

impl From<RtspMountError> for crate::Error {
    fn from(value: RtspMountError) -> Self {
        crate::Error::Output(value.to_string())
    }
}
