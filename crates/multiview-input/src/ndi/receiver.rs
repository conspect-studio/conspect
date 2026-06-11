//! The NDI **receive** seam: a tiny trait an [`super::NdiProducer`] samples, plus
//! a deterministic [`FakeNdiReceiver`] for unit-testing the ingest path without
//! the proprietary SDK.
//!
//! NDI receive is **sampled, never pacing** (invariants #1 / #2): the producer
//! pulls "the latest frame if one is ready" each call; an NDI receive that has no
//! frame this instant returns [`ReceivedFrame::None`] (the `NDIlib_recv_capture`
//! timeout case) — it never blocks the caller. The real implementation binds this
//! trait onto the resolved `NDIlib_v6_load` function table
//! (`NDIlib_recv_create_v3` + `NDIlib_recv_capture_v3` + `NDIlib_recv_destroy`),
//! wrapped in **`FrameSync`** for per-source timing — a **live-only** concern that
//! needs the SDK ABI and a running NDI network. The seam is kept tiny so the drive
//! loop (sampling, conversion, last-good, reconnect) is fully testable over the
//! fake here.

use super::convert::ReceivedVideoFrame;

/// The host-buffer pixel layout (`FourCC`) of a received NDI video frame this
/// crate converts to NV12.
///
/// NDI's low-latency default is [`NdiRecvFourCc::Uyvy`] (8-bit 4:2:2 packed);
/// [`NdiRecvFourCc::Bgra`] is used for keying/overlay sources. Other received
/// layouts (e.g. P216, the 16-bit quality path) are surfaced as a typed refusal
/// by the converter rather than mishandled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiRecvFourCc {
    /// 8-bit 4:2:2 packed `Y'CbCr` — the NDI `color_format_fastest` low-latency
    /// default.
    Uyvy,
    /// 8-bit BGRA (with alpha) — keying / overlay sources.
    Bgra,
}

/// One sampling outcome from an [`NdiReceiver`].
///
/// Modelled `#[non_exhaustive]` so audio / metadata receive outcomes can be added
/// later without breaking the match.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceivedFrame {
    /// A video frame is ready this sample.
    Video(ReceivedVideoFrame),
    /// No frame was ready this sample (the `NDIlib_recv_capture` timeout). The
    /// producer surfaces this as "no frame this tick" — it never blocks or spins.
    None,
}

/// A typed failure from the NDI receive seam.
///
/// A receive fault is a *reported* outcome the supervisor reacts to with the
/// reconnect backoff — never a panic. `#[non_exhaustive]` for additive variants.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NdiRecvError {
    /// The receiver could not be created / connected to the named source (e.g. the
    /// SDK refused, or the source is not currently on the network). The producer
    /// treats this as a connection fault and reconnects.
    #[error("ndi receiver connect failed: {detail}")]
    ConnectFailed {
        /// Human-readable detail.
        detail: String,
    },
    /// The source disconnected / the receiver was closed. The producer treats this
    /// as end-of-stream and reconnects (a live source) or stops (a finite one).
    #[error("ndi source disconnected")]
    Disconnected,
}

/// The minimal NDI receive seam an [`super::NdiProducer`] samples.
///
/// `receive` is a **non-blocking sample**: it returns the latest frame if one is
/// ready, [`ReceivedFrame::None`] when none is (the timeout case), or an
/// [`NdiRecvError`] on a fault the supervisor reconnects from. The seam is
/// intentionally tiny: one named source → one video stream, host-memory frames
/// only.
pub trait NdiReceiver {
    /// Sample the next received frame without blocking.
    ///
    /// # Errors
    /// [`NdiRecvError`] on a connect/disconnect fault the supervisor reconnects
    /// from.
    fn receive(&mut self) -> Result<ReceivedFrame, NdiRecvError>;
}

/// A deterministic in-memory [`NdiReceiver`] for unit-testing the ingest path
/// without the proprietary SDK.
///
/// Plays back a scripted sequence of [`ReceivedFrame`]s (and optionally a fault),
/// so tests can assert the producer's sampling, conversion, last-good, and
/// reconnect behaviour — all without any FFI. Once the script is exhausted it
/// returns [`ReceivedFrame::None`] forever (a quiet source), modelling an NDI
/// receive that simply has no new frame.
#[derive(Debug, Default)]
pub struct FakeNdiReceiver {
    frames: std::collections::VecDeque<ReceivedFrame>,
    /// If set, the receiver returns this error once the scripted frames are
    /// exhausted (to exercise the reconnect path), then keeps returning it.
    fault_after_frames: Option<NdiRecvError>,
}

impl FakeNdiReceiver {
    /// A fake that plays back `frames` in order, then [`ReceivedFrame::None`]
    /// forever.
    #[must_use]
    pub fn with_frames(frames: Vec<ReceivedFrame>) -> Self {
        Self {
            frames: frames.into(),
            fault_after_frames: None,
        }
    }

    /// A fake that plays back `frames`, then faults with `error` (and keeps
    /// faulting) — to drive the producer's reconnect bracket in a test.
    #[must_use]
    pub fn with_frames_then_fault(frames: Vec<ReceivedFrame>, error: NdiRecvError) -> Self {
        Self {
            frames: frames.into(),
            fault_after_frames: Some(error),
        }
    }
}

impl NdiReceiver for FakeNdiReceiver {
    fn receive(&mut self) -> Result<ReceivedFrame, NdiRecvError> {
        if let Some(frame) = self.frames.pop_front() {
            return Ok(frame);
        }
        match &self.fault_after_frames {
            Some(err) => Err(err.clone()),
            // A quiet source: no frame ready, no fault — the timeout case.
            None => Ok(ReceivedFrame::None),
        }
    }
}
