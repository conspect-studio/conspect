//! The safe NDI **sender** handle (ADR-0028 §2/§3).
//!
//! [`NdiSender`] is the *only* way the rest of the workspace pushes frames to NDI.
//! It wraps the resolved [`NdiV6`] table plus the SDK's opaque send instance and
//! exposes a tiny, panic-free surface: create a named sender, send one host-memory
//! video frame, and release the handle on `Drop`. All `unsafe` is confined here;
//! the consuming `multiview-output` sink stays `forbid(unsafe_code)`.
//!
//! ## Clocking (inv #1 / #3)
//! Senders are created with `clock_video = clock_audio = false`: NDI is a pure
//! sink and **must never pace Multiview** — our fixed-cadence tick is the sole
//! clock, and every frame's `timecode` is re-stamped from that tick upstream,
//! never raw input PTS.
//!
//! ## Send semantics (this slice = synchronous)
//! [`NdiSender::send_video`] uses `NDIlib_send_send_video_v2` — the SDK copies the
//! host buffer **before returning**, so there is no buffer-lifetime hazard and the
//! borrowed `&[u8]` is sound. The zero-copy `*_async_v2` path (ADR-0028 §2's
//! `AsyncSendGuard` typestate, which pins the buffer until the next send) is a
//! later optimisation that needs a seam change; its table slot is already resolved
//! ([`NdiV6::send_send_video_async_v2`]) for when it lands.

use std::ffi::CString;
use std::os::raw::c_int;

use crate::error::NdiError;
use crate::ffi;
use crate::table::{NdiV6, SendInstance};
use crate::NdiApiTable;

/// The host-buffer pixel layout (`FourCC`) a [`NdiSender`] send frame declares.
///
/// The live default is [`NdiVideoFourCc::Uyvy`] (NDI's low-latency 4:2:2 packed
/// format), produced by the compositor's NV12→UYVY host copy. The other variants
/// map the remaining layouts the SDK accepts so the safe boundary is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NdiVideoFourCc {
    /// 8-bit 4:2:2 packed `Y'CbCr` (`UYVY`) — the low-latency default.
    Uyvy,
    /// 8-bit BGRA with alpha (`BGRA`) — keying / overlay sources.
    Bgra,
    /// 8-bit 4:2:0 semi-planar (`NV12`) — the native-canvas zero-copy candidate.
    Nv12,
    /// 16-bit 4:2:2 (`P216`) — the quality / HDR layout.
    P216,
}

impl NdiVideoFourCc {
    /// The raw `bindgen` `FourCC` value the SDK expects.
    fn as_raw(self) -> ffi::NDIlib_FourCC_video_type_e {
        match self {
            Self::Uyvy => ffi::NDIlib_FourCC_video_type_UYVY,
            Self::Bgra => ffi::NDIlib_FourCC_video_type_BGRA,
            Self::Nv12 => ffi::NDIlib_FourCC_video_type_NV12,
            Self::P216 => ffi::NDIlib_FourCC_video_type_P216,
        }
    }

    /// Total host bytes the SDK reads for a `stride`×`height` frame of this layout.
    ///
    /// Packed layouts (UYVY/BGRA/P216) are `stride * height`; semi-planar NV12 adds
    /// the half-height interleaved chroma plane (`stride * height / 2`).
    fn buffer_len(self, stride: u64, height: u64) -> u64 {
        let base = stride.saturating_mul(height);
        match self {
            Self::Nv12 => base.saturating_add(base / 2),
            Self::Uyvy | Self::Bgra | Self::P216 => base,
        }
    }
}

/// A safe single-source NDI **sender** over the resolved v6 function table.
///
/// Construct with [`NdiSender::create`]; push frames with [`NdiSender::send_video`];
/// the SDK handle is released exactly once on `Drop`. The wrapped fn pointers and
/// instance are valid only while the [`crate::NdiRuntime`] that produced the
/// `table` stays alive — the caller (the output sink) keeps that runtime alongside
/// the sender, so it always outlives this handle.
pub struct NdiSender {
    table: NdiV6,
    instance: SendInstance,
}

// SAFETY: an NDI send instance is a heap handle the SDK accesses from one thread
// at a time. Transferring *ownership* across threads is sound — we never share it
// (`NdiSender` is left `!Sync` by the raw-pointer field, so it can't be used from
// two threads at once). The resolved fn pointers in `table` are process-lifetime.
#[allow(unsafe_code)]
unsafe impl Send for NdiSender {}

impl std::fmt::Debug for NdiSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The resolved `NdiV6` is bare fn pointers (no `Debug`); show the opaque
        // instance pointer only, so a `Result<NdiSender, _>` is printable.
        f.debug_struct("NdiSender")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl NdiSender {
    /// Create a named NDI sender over the loaded `table`.
    ///
    /// `clock_video`/`clock_audio` should be `false` for Multiview (inv #1/#3:
    /// NDI must never pace us). Returns a typed [`NdiError`] — never a panic — if
    /// the name has an interior NUL or the runtime returns a null handle.
    ///
    /// # Errors
    /// [`NdiError::Table`] if a required fn pointer is missing;
    /// [`NdiError::InvalidCString`] if `name` contains a NUL byte;
    /// [`NdiError::NullInstance`] if the runtime refuses the sender.
    // reason: the FFI create call — deref the resolved table fn pointer and the
    // SDK create struct (// SAFETY below). All other logic is safe.
    #[allow(unsafe_code)]
    pub fn create(
        table: NdiApiTable,
        name: &str,
        clock_video: bool,
        clock_audio: bool,
    ) -> Result<Self, NdiError> {
        let v6 = NdiV6::resolve(table)?;
        // Initialise the runtime so the sender is advertised for discovery (mDNS).
        let _ = v6.ensure_initialized();
        let c_name = CString::new(name).map_err(|_| NdiError::InvalidCString {
            field: "sender name",
        })?;
        let create_t = ffi::NDIlib_send_create_t {
            p_ndi_name: c_name.as_ptr(),
            p_groups: std::ptr::null(),
            clock_video,
            clock_audio,
        };
        // SAFETY: `v6.send_create` is the resolved `NDIlib_send_create` fn pointer
        // (process-lifetime, owned by the live runtime). We pass a pointer to a
        // fully-initialised `create_t` whose `p_ndi_name` points at `c_name`, kept
        // alive across the call (dropped only at scope end); the SDK copies the
        // name. The call returns an owned instance handle (or null), which we
        // null-check below — nothing is dereferenced here.
        let instance = unsafe { (v6.send_create)(std::ptr::from_ref(&create_t)) };
        if instance.is_null() {
            return Err(NdiError::NullInstance { what: "NDI sender" });
        }
        Ok(Self {
            table: v6,
            instance,
        })
    }

    /// Send one host-memory video frame **synchronously**.
    ///
    /// `timecode` is in NDI 100 ns units, already re-stamped from the tick counter
    /// upstream (inv #3). `frame_rate_n`/`frame_rate_d` are the exact rational
    /// cadence (never float fps). `data` is the packed host buffer for `fourcc`;
    /// it must be at least the geometry's length or the send is refused. The SDK
    /// copies the buffer before returning, so `data` need only outlive this call.
    ///
    /// # Errors
    /// [`NdiError::ShortBuffer`] if `data` is too small for the geometry;
    /// [`NdiError::FieldOutOfRange`] if a dimension/rate exceeds the SDK's C `int`.
    // reason: the FFI send call — deref the resolved table fn pointer and pass the
    // frame descriptor (// SAFETY below). The descriptor build is safe.
    #[allow(unsafe_code, clippy::too_many_arguments)]
    pub fn send_video(
        &self,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: NdiVideoFourCc,
        frame_rate_n: u32,
        frame_rate_d: u32,
        timecode: i64,
        data: &[u8],
    ) -> Result<(), NdiError> {
        let need = fourcc.buffer_len(u64::from(stride), u64::from(height));
        let have = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if have < need {
            return Err(NdiError::ShortBuffer {
                have: data.len(),
                need: usize::try_from(need).unwrap_or(usize::MAX),
            });
        }
        let frame = ffi::NDIlib_video_frame_v2_t {
            xres: to_cint(width, "width")?,
            yres: to_cint(height, "height")?,
            FourCC: fourcc.as_raw(),
            frame_rate_N: to_cint(frame_rate_n, "frame_rate_n")?,
            frame_rate_D: to_cint(frame_rate_d, "frame_rate_d")?,
            // 0 lets the SDK derive the aspect ratio from xres/yres.
            picture_aspect_ratio: 0.0,
            frame_format_type: ffi::NDIlib_frame_format_type_progressive,
            timecode,
            // The SDK reads (and copies) this buffer during the synchronous send;
            // it does not mutate it. `cast_mut` satisfies the ABI's `*mut u8`.
            p_data: data.as_ptr().cast_mut(),
            __bindgen_anon_1: ffi::NDIlib_video_frame_v2_t__bindgen_ty_1 {
                line_stride_in_bytes: to_cint(stride, "stride")?,
            },
            p_metadata: std::ptr::null(),
            timestamp: 0,
        };
        // SAFETY: `self.table.send_send_video_v2` is the resolved synchronous send
        // fn pointer; `self.instance` is the live sender from `create`. `frame` is
        // fully initialised and its `p_data` points at `data`, whose length we
        // checked covers the declared geometry. The synchronous send copies the
        // buffer before returning, so `data` (borrowed for this call) outlives the
        // SDK's read. No Rust value is moved into the SDK.
        unsafe {
            (self.table.send_send_video_v2)(self.instance, std::ptr::from_ref(&frame));
        }
        Ok(())
    }

    /// Send one frame of **planar 32-bit float** (`FLTP`) audio synchronously.
    ///
    /// `data` is `no_channels` contiguous planes of `no_samples` `f32` each (plane
    /// 0 then plane 1 …), the canonical NDI audio layout. `timecode` is in NDI
    /// 100 ns units, re-stamped from the tick upstream (inv #3). The SDK copies the
    /// buffer before returning, so `data` need only outlive this call.
    ///
    /// # Errors
    /// [`NdiError::ShortBuffer`] if `data` is smaller than `no_channels *
    /// no_samples`; [`NdiError::FieldOutOfRange`] if a count exceeds the SDK's C
    /// `int` (or the channel stride overflows).
    // reason: the FFI audio-send call — deref the resolved fn pointer and pass the
    // audio descriptor (// SAFETY below). The descriptor build is safe.
    #[allow(unsafe_code)]
    pub fn send_audio(
        &self,
        sample_rate: u32,
        no_channels: u32,
        no_samples: u32,
        timecode: i64,
        data: &[f32],
    ) -> Result<(), NdiError> {
        let need = u64::from(no_channels).saturating_mul(u64::from(no_samples));
        let have = u64::try_from(data.len()).unwrap_or(u64::MAX);
        if have < need {
            return Err(NdiError::ShortBuffer {
                have: data.len(),
                need: usize::try_from(need).unwrap_or(usize::MAX),
            });
        }
        // FLTP planes are tightly packed: stride = no_samples * 4 bytes.
        let stride_bytes = no_samples.checked_mul(4).ok_or(NdiError::FieldOutOfRange {
            field: "audio channel stride",
        })?;
        let frame = ffi::NDIlib_audio_frame_v3_t {
            sample_rate: to_cint(sample_rate, "sample_rate")?,
            no_channels: to_cint(no_channels, "no_channels")?,
            no_samples: to_cint(no_samples, "no_samples")?,
            timecode,
            FourCC: ffi::NDIlib_FourCC_audio_type_FLTP,
            // The SDK reads (and copies) this planar-f32 buffer during the send.
            p_data: data.as_ptr().cast::<u8>().cast_mut(),
            __bindgen_anon_1: ffi::NDIlib_audio_frame_v3_t__bindgen_ty_1 {
                channel_stride_in_bytes: to_cint(stride_bytes, "channel_stride")?,
            },
            p_metadata: std::ptr::null(),
            timestamp: 0,
        };
        // SAFETY: `self.table.send_send_audio_v3` is the resolved synchronous
        // audio-send fn pointer; `self.instance` is the live sender. `frame` is
        // fully initialised and its `p_data` points at `data`, whose length we
        // checked covers `no_channels * no_samples` f32. The send copies the buffer
        // before returning, so the borrowed `data` outlives the SDK's read.
        unsafe {
            (self.table.send_send_audio_v3)(self.instance, std::ptr::from_ref(&frame));
        }
        Ok(())
    }
}

impl Drop for NdiSender {
    // reason: the FFI destroy call — release the SDK handle exactly once.
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.instance` is the live sender from `create`, destroyed
        // exactly once here (the only `send_destroy` for it). The synchronous send
        // path leaves no outstanding async buffer to flush first. This is host-side
        // teardown, never on the engine hot path (inv #1/#10).
        unsafe {
            (self.table.send_destroy)(self.instance);
        }
    }
}

/// Convert a `u32` geometry/rate value into the SDK's C `int`, refusing (rather
/// than truncating) anything that does not fit — keeps the boundary cast-free.
fn to_cint(value: u32, field: &'static str) -> Result<c_int, NdiError> {
    c_int::try_from(value).map_err(|_| NdiError::FieldOutOfRange { field })
}
