//! The **live** `NdiApi` over the resolved SDK function table (ADR-0028 §3).
//!
//! [`SdkNdiApi`] is the production implementation of the [`NdiApi`] sink seam: it
//! delegates to the safe [`multiview_ndi_sys::NdiSender`] handle (which owns all
//! the `unsafe` FFI), so this crate stays `forbid(unsafe_code)`. It is compiled
//! only under the `ndi-bindings` feature — the one that turns on the sys crate's
//! build-time `bindgen` over the licensed SDK header; plain `--features ndi` keeps
//! the SDK-free load scaffolding and the deterministic [`super::api::FakeNdiApi`].
//!
//! Layering / lifetimes: an `SdkNdiApi` owns the [`NdiCapability`] (which owns the
//! loaded runtime `Library` + function table) **and** the [`NdiSender`] resolved
//! from it. The sender holds fn pointers into that `Library`, so it must drop
//! before the capability — guaranteed here by declaration order (Rust drops fields
//! top-to-bottom): `sender` is declared before `capability`.

use multiview_ndi_sys::{NdiSender, NdiVideoFourCc};

use super::api::{NdiApi, NdiAudioFrame, NdiFourCc, NdiSendError, NdiVideoFrame};
use super::loader::NdiCapability;

/// A live [`NdiApi`] backed by the SDK function table via [`NdiSender`].
///
/// Construct with [`SdkNdiApi::new`] from a loaded [`NdiCapability`], then drive it
/// through the [`NdiApi`] seam exactly like the fake: [`super::output::NdiOutput`]
/// remains generic over `A: NdiApi`, so swapping the fake for this is the only
/// change between a unit test and a live send.
#[derive(Debug)]
pub struct SdkNdiApi {
    /// Resolved on `create_sender`; `None` until then / after `destroy_sender`.
    /// Declared **before** `capability` so it drops first (it borrows nothing but
    /// holds fn pointers into the capability's still-mapped `Library`).
    sender: Option<NdiSender>,
    capability: NdiCapability,
}

impl SdkNdiApi {
    /// Build a live NDI API from a loaded runtime capability. No sender exists
    /// until [`NdiApi::create_sender`] is called.
    #[must_use]
    pub fn new(capability: NdiCapability) -> Self {
        Self {
            sender: None,
            capability,
        }
    }
}

/// Map the sink seam's host-buffer layout to the sys crate's send `FourCC`.
fn map_fourcc(fourcc: NdiFourCc) -> NdiVideoFourCc {
    match fourcc {
        NdiFourCc::Uyvy => NdiVideoFourCc::Uyvy,
        NdiFourCc::P216 => NdiVideoFourCc::P216,
        NdiFourCc::Bgra => NdiVideoFourCc::Bgra,
    }
}

impl NdiApi for SdkNdiApi {
    fn create_sender(&mut self, name: &str) -> Result<(), NdiSendError> {
        // clock_video / clock_audio = false: NDI never paces Multiview — our tick
        // is the sole clock and every timecode is re-stamped from it (inv #1/#3).
        let sender = NdiSender::create(self.capability.runtime().api_table(), name, false, false)
            .map_err(|err| NdiSendError::CreateFailed {
            detail: err.to_string(),
        })?;
        self.sender = Some(sender);
        Ok(())
    }

    fn send_video(&mut self, frame: &NdiVideoFrame<'_>) -> Result<(), NdiSendError> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(NdiSendError::Closed);
        };
        // Reuse the descriptor's own consistency checks (zero dims, stride-vs-width,
        // buffer length) before crossing into the sys handle — a typed refusal, not
        // a panic, so a malformed frame never threatens the output clock (inv #1).
        frame.validate()?;
        sender
            .send_video(
                frame.width,
                frame.height,
                frame.stride,
                map_fourcc(frame.fourcc),
                frame.frame_rate_n,
                frame.frame_rate_d,
                frame.timecode,
                frame.data,
            )
            .map_err(|err| NdiSendError::InvalidFrame {
                detail: err.to_string(),
            })
    }

    fn send_audio(&mut self, frame: &NdiAudioFrame<'_>) -> Result<(), NdiSendError> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(NdiSendError::Closed);
        };
        frame.validate()?;
        sender
            .send_audio(
                frame.sample_rate,
                frame.channels,
                frame.samples,
                frame.timecode,
                frame.data,
            )
            .map_err(|err| NdiSendError::InvalidFrame {
                detail: err.to_string(),
            })
    }

    fn destroy_sender(&mut self) {
        // Dropping the `NdiSender` runs `NDIlib_send_destroy` exactly once. Host-
        // side teardown only — never on the engine hot path (inv #1/#10).
        self.sender = None;
    }
}
