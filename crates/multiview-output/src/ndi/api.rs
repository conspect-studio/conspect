//! The NDI API-table **seam** and the host-memory frame descriptor.
//!
//! NDI sends frames from **host memory** (never GPU surfaces; ADR-0004): a single
//! sender publishes the composited canvas after the one GPU→host copy. This
//! module defines the minimal seam an NDI output drives — create a named sender,
//! push a host-memory video frame, destroy the sender — as a trait ([`NdiApi`])
//! so the sink logic ([`super::output::NdiOutput`]) is testable over a
//! deterministic [`FakeNdiApi`] without the proprietary SDK.
//!
//! The real implementation (binding [`NdiApi`] onto the resolved
//! `NDIlib_v6_load` function table) is a **live-only** concern: it needs the SDK
//! ABI and a running NDI network, so it lives behind the live-only test and is
//! out of scope for this load-scaffolding item (OUT-3). OUT-4 wires the real
//! sender and the NV12→UYVY conversion onto this exact seam.

/// The NDI host-buffer pixel layout (`FourCC`) a sender accepts.
///
/// `fastest` (latency priority — the live-multiview default) maps to
/// [`NdiFourCc::Uyvy`]; `best` (quality/HDR) maps to [`NdiFourCc::P216`]. The
/// canvas is NV12 internally; the NV12→UYVY (or →P216) conversion happens at this
/// host-memory boundary (OUT-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiFourCc {
    /// 8-bit 4:2:2 packed — the `color_format_fastest` low-latency default.
    Uyvy,
    /// 16-bit 4:2:2 — the `color_format_best` quality/HDR layout.
    P216,
    /// 8-bit BGRA (with alpha) — for overlays/keying paths.
    Bgra,
}

impl NdiFourCc {
    /// The four ASCII bytes of the on-wire `FourCC` tag.
    #[must_use]
    pub fn tag(self) -> [u8; 4] {
        match self {
            Self::Uyvy => *b"UYVY",
            Self::P216 => *b"P216",
            Self::Bgra => *b"BGRA",
        }
    }
}

/// A host-memory video frame handed to an NDI sender.
///
/// Borrows the host buffer (`data`) for the duration of the send — the canvas is
/// already copied GPU→host upstream, so this is the *send-side* of the single
/// NDI copy boundary (ADR-0004). Timecode is re-stamped from the tick counter
/// upstream (invariant #3), carried here as an `i64` (NDI's 100 ns units), never
/// raw input PTS.
#[derive(Debug, Clone, Copy)]
pub struct NdiVideoFrame<'a> {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Bytes per row of the (top-of-buffer) plane.
    pub stride: u32,
    /// Host-memory pixel layout.
    pub fourcc: NdiFourCc,
    /// Frame-rate numerator (exact rational; never float fps — invariant #3).
    pub frame_rate_n: u32,
    /// Frame-rate denominator (e.g. 1001 for NTSC; exact rational).
    pub frame_rate_d: u32,
    /// Timecode in NDI 100 ns units, re-stamped from the tick counter. A negative
    /// value (`NDIlib_send_timecode_synthesize`) lets the SDK synthesise it.
    pub timecode: i64,
    /// The borrowed host pixel buffer (`stride * height` bytes for packed UYVY).
    pub data: &'a [u8],
}

impl NdiVideoFrame<'_> {
    /// Validate the descriptor's internal consistency before a send.
    ///
    /// Catches the foot-guns that would otherwise be a UB read in the real SDK:
    /// zero dimensions, a stride too small for the width, a buffer shorter than
    /// `stride * height`, or a zero frame-rate denominator. Returns a typed
    /// [`NdiSendError`] — never panics — so a malformed frame is a *reported*
    /// refusal, consistent with the output-clock invariant.
    ///
    /// # Errors
    /// [`NdiSendError::InvalidFrame`] on any inconsistency.
    pub fn validate(&self) -> Result<(), NdiSendError> {
        if self.width == 0 || self.height == 0 {
            return Err(NdiSendError::InvalidFrame {
                detail: "zero width or height".to_owned(),
            });
        }
        if self.frame_rate_d == 0 {
            return Err(NdiSendError::InvalidFrame {
                detail: "zero frame-rate denominator".to_owned(),
            });
        }
        // Minimum bytes per row of the top-of-buffer plane `stride` describes:
        //   UYVY  — 8-bit 4:2:2 packed: 2 bytes/px.
        //   P216  — 16-bit 4:2:2: the Y plane is 2 bytes/px (16-bit samples).
        //   BGRA  — 8-bit 4 channels: 4 bytes/px.
        let min_stride = match self.fourcc {
            NdiFourCc::Uyvy | NdiFourCc::P216 => self.width.saturating_mul(2),
            NdiFourCc::Bgra => self.width.saturating_mul(4),
        };
        if self.stride < min_stride {
            return Err(NdiSendError::InvalidFrame {
                detail: format!(
                    "stride {} too small for {}px {:?} (need >= {min_stride})",
                    self.stride, self.width, self.fourcc
                ),
            });
        }
        let needed = u64::from(self.stride).saturating_mul(u64::from(self.height));
        // `usize`→`u64` is widening on every target we support (64-bit Linux +
        // macOS); `try_from` keeps it cast-free and is infallible here.
        let have = u64::try_from(self.data.len()).unwrap_or(u64::MAX);
        if have < needed {
            return Err(NdiSendError::InvalidFrame {
                detail: format!("buffer {} bytes < required {needed}", self.data.len()),
            });
        }
        Ok(())
    }
}

/// A host-memory **audio** frame handed to an NDI sender.
///
/// NDI carries audio as **planar 32-bit float** (`FLTP`): `channels` contiguous
/// planes of `samples` `f32` each (plane 0 then plane 1 …). Borrows the host
/// buffer for the duration of the send. `timecode` is re-stamped from the tick
/// counter upstream (invariant #3), in NDI's 100 ns units, never raw input PTS.
#[derive(Debug, Clone, Copy)]
pub struct NdiAudioFrame<'a> {
    /// Sample rate in Hz (e.g. 48000).
    pub sample_rate: u32,
    /// Channel count (the number of planes).
    pub channels: u32,
    /// Samples per channel in this frame.
    pub samples: u32,
    /// Timecode in NDI 100 ns units, re-stamped from the tick counter.
    pub timecode: i64,
    /// The borrowed planar-float buffer (`channels * samples` f32).
    pub data: &'a [f32],
}

impl NdiAudioFrame<'_> {
    /// Validate the descriptor's internal consistency before a send.
    ///
    /// Catches zero rate/channels/samples and a buffer shorter than
    /// `channels * samples` — a typed [`NdiSendError`], never a panic.
    ///
    /// # Errors
    /// [`NdiSendError::InvalidFrame`] on any inconsistency.
    pub fn validate(&self) -> Result<(), NdiSendError> {
        if self.sample_rate == 0 || self.channels == 0 || self.samples == 0 {
            return Err(NdiSendError::InvalidFrame {
                detail: format!(
                    "zero audio geometry ({} Hz, {} ch, {} samples)",
                    self.sample_rate, self.channels, self.samples
                ),
            });
        }
        let needed = u64::from(self.channels).saturating_mul(u64::from(self.samples));
        let have = u64::try_from(self.data.len()).unwrap_or(u64::MAX);
        if have < needed {
            return Err(NdiSendError::InvalidFrame {
                detail: format!(
                    "audio buffer {} samples < required {needed}",
                    self.data.len()
                ),
            });
        }
        Ok(())
    }
}

/// A failure from the NDI sender seam.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiSendError {
    /// The frame descriptor was internally inconsistent (see
    /// [`NdiVideoFrame::validate`]).
    InvalidFrame {
        /// Human-readable detail.
        detail: String,
    },
    /// The sender could not be created (e.g. the SDK refused the name, or the
    /// runtime returned a null sender handle).
    CreateFailed {
        /// Human-readable detail.
        detail: String,
    },
    /// A send was attempted after the sender was closed.
    Closed,
}

impl std::fmt::Display for NdiSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFrame { detail } => write!(f, "invalid NDI frame: {detail}"),
            Self::CreateFailed { detail } => write!(f, "NDI sender create failed: {detail}"),
            Self::Closed => write!(f, "NDI sender is closed"),
        }
    }
}

impl std::error::Error for NdiSendError {}

/// The minimal NDI sender seam an [`super::output::NdiOutput`] drives.
///
/// A real implementation binds these onto the resolved `NDIlib_v6_load` function
/// table (`NDIlib_send_create`, `NDIlib_send_send_video_v2`,
/// `NDIlib_send_destroy`) — a live-only concern. The seam is intentionally tiny:
/// one composited canvas → one sender, host-memory frames only.
pub trait NdiApi {
    /// Create a named NDI sender (the source name other tools discover).
    ///
    /// # Errors
    /// [`NdiSendError::CreateFailed`] if the sender cannot be created.
    fn create_sender(&mut self, name: &str) -> Result<(), NdiSendError>;

    /// Push one host-memory video frame to the sender.
    ///
    /// # Errors
    /// [`NdiSendError`] if the sender is closed or the frame is invalid.
    fn send_video(&mut self, frame: &NdiVideoFrame<'_>) -> Result<(), NdiSendError>;

    /// Push one host-memory **audio** frame (planar float) to the sender.
    ///
    /// # Errors
    /// [`NdiSendError`] if the sender is closed or the frame is invalid.
    fn send_audio(&mut self, frame: &NdiAudioFrame<'_>) -> Result<(), NdiSendError>;

    /// Destroy the sender, releasing the SDK handle.
    fn destroy_sender(&mut self);
}

/// A deterministic in-memory [`NdiApi`] for unit-testing the sink seam without
/// the proprietary SDK.
///
/// Records the created sender name and the metadata of each accepted frame so
/// tests can assert the drive loop, the timecode re-stamping, and the
/// closed/invalid refusals — all without any FFI.
#[derive(Debug, Default)]
pub struct FakeNdiApi {
    /// The name passed to [`NdiApi::create_sender`], if any.
    pub created: Option<String>,
    /// Whether the sender is currently open.
    pub open: bool,
    /// One record per accepted frame: `(width, height, fourcc, timecode)`.
    pub sent: Vec<(u32, u32, NdiFourCc, i64)>,
    /// One record per accepted audio frame: `(sample_rate, channels, samples,
    /// timecode)`.
    pub sent_audio: Vec<(u32, u32, u32, i64)>,
    /// If set, [`NdiApi::create_sender`] returns this error instead of succeeding
    /// (to test the create-failure path).
    pub fail_create: Option<String>,
}

impl FakeNdiApi {
    /// A fresh fake with no sender created.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake whose `create_sender` will fail with `detail`.
    #[must_use]
    pub fn failing_create(detail: impl Into<String>) -> Self {
        Self {
            fail_create: Some(detail.into()),
            ..Self::default()
        }
    }
}

impl NdiApi for FakeNdiApi {
    fn create_sender(&mut self, name: &str) -> Result<(), NdiSendError> {
        if let Some(detail) = self.fail_create.clone() {
            return Err(NdiSendError::CreateFailed { detail });
        }
        self.created = Some(name.to_owned());
        self.open = true;
        Ok(())
    }

    fn send_video(&mut self, frame: &NdiVideoFrame<'_>) -> Result<(), NdiSendError> {
        if !self.open {
            return Err(NdiSendError::Closed);
        }
        frame.validate()?;
        self.sent
            .push((frame.width, frame.height, frame.fourcc, frame.timecode));
        Ok(())
    }

    fn send_audio(&mut self, frame: &NdiAudioFrame<'_>) -> Result<(), NdiSendError> {
        if !self.open {
            return Err(NdiSendError::Closed);
        }
        frame.validate()?;
        self.sent_audio.push((
            frame.sample_rate,
            frame.channels,
            frame.samples,
            frame.timecode,
        ));
        Ok(())
    }

    fn destroy_sender(&mut self) {
        self.open = false;
    }
}
