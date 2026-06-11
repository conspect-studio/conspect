//! The `KmsBackend` trait seam: everything the display sink loop needs from a
//! DRM/KMS device, expressed over plain data so the loop (and CI) run with a
//! scripted mock while the real ioctl-speaking implementation lives in
//! [`super::kms`] behind the `display-kms` feature.

use std::time::Duration;

use thiserror::Error;

use super::canvas::DisplayCanvas;
use super::mode::{DisplayModeInfo, ModeError};

/// One KMS connector as probed: kernel name (`DP-1`, `HDMI-A-1`), connection
/// state, and its EDID-advertised modes (empty = EDID-less).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorDesc {
    /// Kernel connector name (`<interface>-<id>`).
    pub name: String,
    /// Whether a sink is attached.
    pub connected: bool,
    /// EDID-advertised modes; empty for an EDID-less chain.
    pub modes: Vec<DisplayModeInfo>,
}

/// Which connector a sink drives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectorSelector {
    /// The first connected connector.
    Auto,
    /// A connector by kernel name (`DP-1`).
    Name(String),
}

/// The resolved head: connector + the timing to commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadSetup {
    /// Kernel connector name.
    pub connector: String,
    /// The selected timing.
    pub mode: DisplayModeInfo,
    /// `true` when the timing came from EDID; `false` for a forced CVT-RB
    /// timing (EDID-less head — no ELD, so no audio path either).
    pub from_edid: bool,
}

/// One page-flip-completion event (the kernel's `DRM_EVENT_FLIP_COMPLETE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlipEvent {
    /// The CRTC frame counter at completion.
    pub crtc_frame: u32,
    /// The kernel's flip timestamp (monotonic-clock based) — the
    /// presentation-skew telemetry source (flip-event-only v1; see the
    /// module-level spike verdict).
    pub timestamp: Duration,
}

/// A nonblocking frame commit outcome that is not success.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SubmitError {
    /// The kernel already has a commit in flight on this CRTC (`EBUSY`).
    /// This **is** mailbox conflation: the caller waits for the next flip
    /// event and then commits the latest frame — never queues, never spins.
    #[error("a commit is already in flight on this CRTC (EBUSY)")]
    Busy,
    /// Any other device failure; the sink holds the last-good framebuffer on
    /// glass (KMS repeats it) and keeps running.
    #[error("display device: {0}")]
    Device(DisplayError),
}

/// Display sink failures (startup and device-level).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DisplayError {
    /// The named connector does not exist on the device.
    #[error("connector {requested:?} not found (available: {available:?})")]
    ConnectorNotFound {
        /// The requested connector name.
        requested: String,
        /// The connector names the device exposes.
        available: Vec<String>,
    },
    /// `auto` was requested but no connector has a sink attached.
    #[error("no connected connector found (probed: {probed:?})")]
    NoneConnected {
        /// The connector names probed.
        probed: Vec<String>,
    },
    /// The named connector exists but nothing is attached to it.
    #[error("connector {name:?} is not connected")]
    NotConnected {
        /// The connector name.
        name: String,
    },
    /// Mode selection failed (see [`ModeError`]).
    #[error("mode selection: {0}")]
    Mode(#[from] ModeError),
    /// An ioctl/device-level failure, with context.
    #[error("kms device: {0}")]
    Device(String),
}

/// What the flip loop needs from a DRM/KMS device. Implemented by the real
/// drm-rs backend ([`super::kms`], feature `display-kms`) and by scripted
/// mocks in tests — the loop's behaviour (conflation, EBUSY handling, modeset
/// discipline) is CI-proven hardware-free through this seam.
pub trait KmsBackend: Send {
    /// Enumerate connectors (a forced probe — startup / explicit
    /// reconfiguration only, never the frame path).
    ///
    /// # Errors
    ///
    /// [`DisplayError::Device`] when the device cannot be probed.
    fn probe_connectors(&mut self) -> Result<Vec<ConnectorDesc>, DisplayError>;

    /// Validate the full configuration with a `TEST_ONLY | ALLOW_MODESET`
    /// atomic commit — no hardware state is touched. Runs at startup before
    /// the one real modeset.
    ///
    /// # Errors
    ///
    /// [`DisplayError`] when the device rejects the plane/format/mode
    /// combination.
    fn validate_setup(&mut self, setup: &HeadSetup) -> Result<(), DisplayError>;

    /// Perform the one startup `ALLOW_MODESET` commit: allocate the scanout
    /// buffers, program the mode, and light the pipe (black). Blocking;
    /// startup / Class-2 reconfiguration only — **never** the frame path.
    ///
    /// # Errors
    ///
    /// [`DisplayError`] when the modeset fails.
    fn apply_modeset(&mut self, setup: &HeadSetup) -> Result<(), DisplayError>;

    /// Write `frame` into a free scanout buffer and submit
    /// `atomic_commit(NONBLOCK | PAGE_FLIP_EVENT)`.
    ///
    /// # Errors
    ///
    /// [`SubmitError::Busy`] when a commit is already in flight (conflation);
    /// [`SubmitError::Device`] for anything else (the sink holds last-good).
    fn submit_frame(&mut self, frame: &dyn DisplayCanvas) -> Result<(), SubmitError>;

    /// Wait up to `timeout` for device events, returning any page-flip
    /// completions. A bounded wait — the loop also uses the timeout to notice
    /// new mailbox frames while the pipe is idle.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Device`] on an unrecoverable event-channel failure.
    fn wait_events(&mut self, timeout: Duration) -> Result<Vec<FlipEvent>, DisplayError>;
}
