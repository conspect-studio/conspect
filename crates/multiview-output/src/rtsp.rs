//! RTSP egress — the OUT-1 **sidecar baseline** publish seam.
//!
//! # The decision (ADR-0006)
//!
//! Two RTSP egress paths exist in the design:
//!
//! * **Primary (OUT-2):** an in-process `gst-rtsp-server` that fans the
//!   already-encoded canvas to RTSP clients with no re-encode. It pulls the
//!   GStreamer/GLib C stack, so it lives behind a future `rtsp` Cargo feature and
//!   is **not** this module.
//! * **Baseline (OUT-1, here):** the **publish hop** — libav RTSP ANNOUNCE/RECORD
//!   to a *listening* RTSP endpoint such as a local
//!   [MediaMTX](https://github.com/bluenviron/mediamtx) sidecar. This reuses the
//!   existing `PushProtocol::Rtsp` push
//!   transport (which already maps to libav's `rtsp` muxer) — **zero new sink
//!   code** — and stays LGPL-clean and native-light (no `GLib`).
//!
//! This module owns the one piece the baseline still needs: a typed,
//! pure-Rust way to derive the publish URL from a configured base + mount. It is
//! **always compiled** (no `ffmpeg`/native dependency, so it is always
//! CI-tested); the coupling to the push transport — selecting
//! `PushProtocol::Rtsp` — is the only part
//! gated behind the `ffmpeg` feature.
//!
//! # The publish URL
//!
//! A deploy configures a publish *base* (host + port of the RTSP sidecar, e.g.
//! the `MediaMTX` default `rtsp://[::1]:8554`) and each output names a *mount*
//! (the RTSP path the program is published under). [`RtspPublishTarget`] joins
//! them into the `rtsp://host:port/mount` URL the `PushSink`
//! opens, using **checked string formatting only** — no panics, no indexing, no
//! `as` casts (guardrails). The base must be an RTSP(S) scheme with no path of its
//! own; the mount must be a non-empty, whitespace-free RTSP path.
//!
//! # Invariants
//!
//! The published stream is the **same** one-encode canvas the file/HLS/UDP sinks
//! carry (invariant #7) — this seam only chooses *where* to push it. A dropped or
//! absent RTSP sidecar surfaces as a typed
//! [`Error::Output`](crate::Error::Output) at connect time on the push sink's own
//! thread; it never stalls the output clock (#1) or back-pressures the engine
//! (#10), exactly like every other `PushSink` target.

use thiserror::Error;

#[cfg(feature = "ffmpeg")]
use crate::sink::PushProtocol;

/// A validated RTSP **publish** target for the OUT-1 sidecar baseline.
///
/// Built from a publish *base* (`rtsp://host:port`, the listening sidecar) and a
/// *mount* (the RTSP path), it exposes the joined [`publish_url`](Self::publish_url)
/// a `PushSink` opens. Construction validates both parts
/// up front so a misconfigured base/mount is a typed error, never a silently-wrong
/// URL handed to libav.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspPublishTarget {
    /// The fully-joined publish URL (`rtsp://host:port/mount`), built once at
    /// construction.
    publish_url: String,
    /// The normalized mount (no leading/trailing slashes), retained for display
    /// and routing.
    mount: String,
}

impl RtspPublishTarget {
    /// The default publish base: a local [MediaMTX] sidecar's RTSP listener.
    ///
    /// `MediaMTX` (and most RTSP servers) listen on TCP `8554` by default; the
    /// IPv6 loopback (`[::1]`) keeps the baseline a local publish hop. IPv6-first
    /// (operator directive): a user-supplied IPv4 base (`rtsp://127.0.0.1:8554`)
    /// still builds, but is never the default. A deploy overrides this via its
    /// `[system.rtsp] publish_base` configuration (wired by the CLI).
    ///
    /// [MediaMTX]: https://github.com/bluenviron/mediamtx
    pub const DEFAULT_BASE: &'static str = "rtsp://[::1]:8554";

    /// Build a publish target from `base` (`rtsp://host:port`) and `mount` (the
    /// RTSP path the program is published under).
    ///
    /// The base and mount are joined with exactly one `/` separator regardless of
    /// a trailing slash on the base or leading slash on the mount; interior mount
    /// slashes (multi-segment paths) are preserved.
    ///
    /// # Errors
    ///
    /// Returns an [`RtspPublishError`] when:
    ///
    /// * the base is empty ([`EmptyBase`](RtspPublishError::EmptyBase)),
    /// * the base is not an `rtsp://`/`rtsps://` scheme
    ///   ([`NotRtspScheme`](RtspPublishError::NotRtspScheme)),
    /// * the base has the scheme but no `host:port` authority, e.g. a bare
    ///   `rtsp://` ([`MissingAuthority`](RtspPublishError::MissingAuthority)),
    /// * the base already carries a path past `host:port`
    ///   ([`BaseHasPath`](RtspPublishError::BaseHasPath)),
    /// * the mount is empty or only slashes
    ///   ([`EmptyMount`](RtspPublishError::EmptyMount)), or
    /// * the mount contains whitespace or control characters
    ///   ([`InvalidMount`](RtspPublishError::InvalidMount)).
    pub fn new(base: impl AsRef<str>, mount: impl AsRef<str>) -> Result<Self, RtspPublishError> {
        let base = base.as_ref();
        let mount = mount.as_ref();

        if base.is_empty() {
            return Err(RtspPublishError::EmptyBase);
        }

        // The authority (`host:port`) after the validated scheme; the remainder of
        // the base must be empty or a bare trailing slash — never a path.
        let authority = strip_rtsp_scheme(base).ok_or_else(|| RtspPublishError::NotRtspScheme {
            base: base.to_owned(),
        })?;
        // The authority must carry a real `host[:port]`; a bare `rtsp://` (empty
        // authority) would otherwise join into a host-less `rtsp:/mount` URL —
        // a silently-wrong target the typed builder must reject (no panic).
        if authority.trim_end_matches('/').is_empty() {
            return Err(RtspPublishError::MissingAuthority {
                base: base.to_owned(),
            });
        }
        // A path on the base is ambiguous with the mount, so reject it (a bare
        // trailing slash is fine and is trimmed below).
        if authority.trim_end_matches('/').contains('/') {
            return Err(RtspPublishError::BaseHasPath {
                base: base.to_owned(),
            });
        }

        // Normalize the mount: drop surrounding slashes, then validate the path.
        let normalized_mount = mount.trim_matches('/');
        if normalized_mount.is_empty() {
            return Err(RtspPublishError::EmptyMount);
        }
        if normalized_mount
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(RtspPublishError::InvalidMount {
                mount: mount.to_owned(),
            });
        }

        // Join with exactly one separator: trim any trailing slash off the base,
        // then append `/{mount}`. Checked formatting only (no indexing/`as`).
        let base_no_trailing = base.trim_end_matches('/');
        let publish_url = format!("{base_no_trailing}/{normalized_mount}");

        Ok(Self {
            publish_url,
            mount: normalized_mount.to_owned(),
        })
    }

    /// Build a publish target against [`DEFAULT_BASE`](Self::DEFAULT_BASE) (the
    /// local `MediaMTX` sidecar) for `mount`.
    ///
    /// # Errors
    ///
    /// Returns an [`RtspPublishError`] when `mount` is empty/all-slashes or
    /// contains whitespace (see [`RtspPublishTarget::new`]).
    pub fn with_default_base(mount: impl AsRef<str>) -> Result<Self, RtspPublishError> {
        Self::new(Self::DEFAULT_BASE, mount)
    }

    /// The fully-joined publish URL (`rtsp://host:port/mount`) a
    /// `PushSink` opens.
    #[must_use]
    pub fn publish_url(&self) -> &str {
        &self.publish_url
    }

    /// The normalized mount path (no surrounding slashes).
    #[must_use]
    pub fn mount(&self) -> &str {
        &self.mount
    }

    /// The push protocol this target streams through — always
    /// `PushProtocol::Rtsp`, which selects
    /// libav's `rtsp` muxer (the OUT-1 baseline reuses the existing push path; no
    /// new sink code).
    ///
    /// Available only under the `ffmpeg` feature, where the push transport exists.
    #[cfg(feature = "ffmpeg")]
    #[must_use]
    pub const fn protocol(&self) -> PushProtocol {
        PushProtocol::Rtsp
    }
}

/// Strip a validated `rtsp://` / `rtsps://` scheme prefix, returning the
/// authority-and-beyond remainder, or `None` if the scheme is not RTSP(S).
///
/// Pure string slicing on ASCII scheme literals — never indexes by a computed
/// offset.
fn strip_rtsp_scheme(base: &str) -> Option<&str> {
    base.strip_prefix("rtsp://")
        .or_else(|| base.strip_prefix("rtsps://"))
}

/// Why an [`RtspPublishTarget`] could not be built from a base + mount.
///
/// `#[non_exhaustive]`: downstream `match` must carry a wildcard so new
/// validation arms are additive.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RtspPublishError {
    /// The publish base was empty.
    #[error("rtsp publish base is empty")]
    EmptyBase,

    /// The publish base did not use an `rtsp://` or `rtsps://` scheme. The RTSP
    /// push protocol is fixed, so a non-RTSP base is a configuration error rather
    /// than a silently-wrong URL.
    #[error("rtsp publish base must be an rtsp:// or rtsps:// url, got `{base}`")]
    NotRtspScheme {
        /// The offending base.
        base: String,
    },

    /// The publish base had an `rtsp(s)://` scheme but no `host:port` authority
    /// (e.g. a bare `rtsp://`). Joining a mount onto it would produce a host-less
    /// URL, so it is rejected rather than handed to libav.
    #[error("rtsp publish base has no host:port authority, got `{base}`")]
    MissingAuthority {
        /// The offending base.
        base: String,
    },

    /// The publish base carried a path past `host:port`. The base is the sidecar
    /// authority only; the path belongs in the mount, so a base with a path is
    /// ambiguous and rejected (no silent concatenation).
    #[error("rtsp publish base must be host:port only (no path), got `{base}`")]
    BaseHasPath {
        /// The offending base.
        base: String,
    },

    /// The mount was empty or contained only slashes — there is no RTSP path to
    /// publish to.
    #[error("rtsp mount is empty")]
    EmptyMount,

    /// The mount contained whitespace or control characters, which would produce
    /// an invalid RTSP URL.
    #[error("rtsp mount `{mount}` contains whitespace or control characters")]
    InvalidMount {
        /// The offending mount.
        mount: String,
    },
}

impl From<RtspPublishError> for crate::Error {
    fn from(value: RtspPublishError) -> Self {
        // A misconfigured publish target is an output-side failure.
        crate::Error::Output(value.to_string())
    }
}
