//! The **live** `NdiReceiver` over the resolved SDK function table (ADR-0028 §3).
//!
//! [`SdkNdiReceiver`] is the production implementation of the [`NdiReceiver`]
//! receive seam: it delegates to the safe [`multiview_ndi_sys::NdiReceiver`] handle
//! (which owns all the `unsafe` FFI + the free-exactly-once RAII), so this crate
//! stays `forbid(unsafe_code)`. It is compiled only under the `ndi-bindings`
//! feature — the one that turns on the sys crate's build-time `bindgen` over the
//! licensed SDK header; plain `--features ndi` keeps the SDK-free scaffolding and
//! the deterministic [`super::receiver::FakeNdiReceiver`].
//!
//! Sampled, never pacing (inv #1/#2/#10): each [`NdiReceiver::receive`] is a
//! bounded, non-blocking capture (a short SDK timeout on the ingest thread, not the
//! engine). A capture with no frame this instant is [`ReceivedFrame::None`] — the
//! producer holds last-good and the tile rides its state machine; nothing blocks.
//!
//! Layering / lifetimes: an `SdkNdiReceiver` owns the [`NdiCapability`] (the loaded
//! runtime `Library` + table) **and** the [`multiview_ndi_sys::NdiReceiver`]
//! resolved from it. The receiver holds fn pointers into that `Library`, so it must
//! drop first — guaranteed by declaration order (`receiver` before `capability`).

use multiview_ndi_sys::{NdiReceiver as SysReceiver, RecvFourCc};

use super::convert::ReceivedVideoFrame;
use super::loader::NdiCapability;
use super::receiver::{NdiReceiver, NdiRecvError, NdiRecvFourCc, ReceivedFrame};

/// How long a single capture waits for a frame before reporting "none this tick".
/// Short enough to stay responsive on the ingest thread, long enough to avoid a
/// busy-spin between ticks.
const CAPTURE_TIMEOUT_MS: u32 = 16;

/// A live [`NdiReceiver`] backed by the SDK function table via the sys
/// [`multiview_ndi_sys::NdiReceiver`].
///
/// Construct with [`SdkNdiReceiver::connect`] from a loaded [`NdiCapability`] and a
/// source name; then drive it through the [`NdiReceiver`] seam exactly like the
/// fake — [`super::producer::NdiProducer`] is generic over `dyn NdiReceiver`, so
/// swapping the fake for this is the only change between a unit test and live
/// ingest.
#[derive(Debug)]
pub struct SdkNdiReceiver {
    /// Declared **before** `capability` so it drops first (it holds fn pointers
    /// into the capability's still-mapped `Library`).
    receiver: SysReceiver,
    capability: NdiCapability,
}

impl SdkNdiReceiver {
    /// Connect a receiver to the NDI source named `source_name`.
    ///
    /// # Errors
    /// [`NdiRecvError::ConnectFailed`] if the runtime refuses the receiver (e.g. a
    /// malformed name or a null handle) — the supervisor reconnects from this.
    pub fn connect(capability: NdiCapability, source_name: &str) -> Result<Self, NdiRecvError> {
        let receiver = SysReceiver::create(
            capability.runtime().api_table(),
            source_name,
            Some("Multiview"),
        )
        .map_err(|err| NdiRecvError::ConnectFailed {
            detail: err.to_string(),
        })?;
        Ok(Self {
            receiver,
            capability,
        })
    }

    /// Borrow the underlying loaded NDI capability. It is owned by this receiver
    /// (kept mapped for the receiver's lifetime, drop order enforced); exposing it
    /// lets callers read the runtime/probe status without a second load.
    #[must_use]
    pub fn capability(&self) -> &NdiCapability {
        &self.capability
    }
}

impl NdiReceiver for SdkNdiReceiver {
    fn receive(&mut self) -> Result<ReceivedFrame, NdiRecvError> {
        // Capture is infallible at the FFI boundary today (a fault surfaces as
        // `None`); map the `Result` defensively in case that changes.
        let Some(frame) = self
            .receiver
            .capture_video(CAPTURE_TIMEOUT_MS)
            .map_err(|err| NdiRecvError::ConnectFailed {
                detail: err.to_string(),
            })?
        else {
            return Ok(ReceivedFrame::None);
        };

        // Classify the packing; an unrequested layout is dropped as "no frame this
        // tick" (we only ever request UYVY_BGRA, so anything else is not expected).
        // `RecvFourCc` is `#[non_exhaustive]`, so the catch-all also covers future
        // variants.
        let fourcc = match frame.fourcc() {
            RecvFourCc::Uyvy => NdiRecvFourCc::Uyvy,
            RecvFourCc::Bgra => NdiRecvFourCc::Bgra,
            _ => return Ok(ReceivedFrame::None),
        };
        let (width, height, stride, raw_tc) = (
            frame.width(),
            frame.height(),
            frame.stride(),
            frame.timecode(),
        );
        // Copy the pixels out of SDK-owned memory before `frame` drops (which frees
        // it). A negative NDI timecode is the synthesize sentinel → genpts fallback.
        let data = frame.data().to_vec();
        let timecode = if raw_tc < 0 { None } else { Some(raw_tc) };
        drop(frame);

        // A malformed geometry is a typed refusal → "no frame this tick"
        // (sampled-not-pacing): never a panic, never a stall.
        match ReceivedVideoFrame::with_timecode(width, height, fourcc, stride, timecode, data) {
            Ok(received) => Ok(ReceivedFrame::Video(received)),
            Err(_) => Ok(ReceivedFrame::None),
        }
    }
}
