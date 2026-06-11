//! The safe NDI **receiver** handle (ADR-0028 §2/§3).
//!
//! [`NdiReceiver`] connects to a named NDI source and samples video frames; it is
//! the *only* way the workspace receives NDI, and all `unsafe` is confined here so
//! the consuming `multiview-input` producer stays `forbid(unsafe_code)`.
//!
//! ## Sampled, never pacing (inv #1 / #2 / #10)
//! [`NdiReceiver::capture_video`] is a **non-blocking sample** with a short
//! timeout: it returns the latest video frame if one is ready, or `None` on the
//! timeout / a non-video frame type. It never blocks the caller and so can never
//! pace or back-pressure the engine — the producer pulls it each tick and writes
//! last-good.
//!
//! ## Free-exactly-once (RAII)
//! `NDIlib_recv_capture_v3` hands back **SDK-owned** video memory that must be
//! returned via `NDIlib_recv_free_video_v2`. [`RecvVideoFrame`] owns that
//! allocation and frees it on `Drop` **exactly once**. It borrows the receiver
//! (`&'r NdiReceiver`) so the free target (the recv instance) is guaranteed alive
//! by the borrow checker — no dangling free is expressible.

use std::ffi::CString;
use std::mem::MaybeUninit;

use crate::error::NdiError;
use crate::ffi;
use crate::table::{NdiV6, RecvInstance};
use crate::NdiApiTable;

/// The packing (`FourCC`) of a received NDI video frame, classified from the raw
/// SDK value. With the `UYVY_BGRA` color format the SDK delivers [`RecvFourCc::Uyvy`]
/// for opaque sources and [`RecvFourCc::Bgra`] for sources with alpha.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecvFourCc {
    /// 8-bit 4:2:2 packed `Y'CbCr` (`UYVY`).
    Uyvy,
    /// 8-bit BGRA with alpha (`BGRA`).
    Bgra,
    /// Any other layout the SDK delivered (carried raw for a typed refusal upstream).
    Other(u32),
}

impl RecvFourCc {
    fn classify(raw: ffi::NDIlib_FourCC_video_type_e) -> Self {
        if raw == ffi::NDIlib_FourCC_video_type_UYVY {
            Self::Uyvy
        } else if raw == ffi::NDIlib_FourCC_video_type_BGRA {
            Self::Bgra
        } else {
            Self::Other(raw)
        }
    }
}

/// A safe single-source NDI **receiver** over the resolved v6 function table.
///
/// Construct with [`NdiReceiver::create`]; sample frames with
/// [`NdiReceiver::capture_video`]; the SDK handle is released exactly once on
/// `Drop`. Valid only while the [`crate::NdiRuntime`] that produced the `table`
/// stays alive (the consuming producer keeps that runtime alongside this handle).
pub struct NdiReceiver {
    table: NdiV6,
    instance: RecvInstance,
}

// SAFETY: an NDI recv instance is a heap handle the SDK accesses from one thread
// at a time. Transferring *ownership* across threads is sound — we never share it
// (`NdiReceiver` is left `!Sync` by the raw-pointer field). The resolved fn
// pointers in `table` are process-lifetime.
#[allow(unsafe_code)]
unsafe impl Send for NdiReceiver {}

impl NdiReceiver {
    /// Create a receiver connected to the NDI source named `source_name`.
    ///
    /// Uses the low-latency `UYVY_BGRA` color format and highest bandwidth. The
    /// optional `recv_name` is the receiver's own name other tools display.
    ///
    /// # Errors
    /// [`NdiError::Table`] if a required fn pointer is missing;
    /// [`NdiError::InvalidCString`] if a name contains a NUL byte;
    /// [`NdiError::NullInstance`] if the runtime refuses the receiver.
    // reason: the FFI create call — deref the resolved table fn pointer and the SDK
    // create struct (// SAFETY below). All other logic is safe.
    #[allow(unsafe_code)]
    pub fn create(
        table: NdiApiTable,
        source_name: &str,
        recv_name: Option<&str>,
    ) -> Result<Self, NdiError> {
        let v6 = NdiV6::resolve(table)?;
        let _ = v6.ensure_initialized();
        let c_source = CString::new(source_name).map_err(|_| NdiError::InvalidCString {
            field: "source name",
        })?;
        let c_recv = match recv_name {
            Some(name) => Some(CString::new(name).map_err(|_| NdiError::InvalidCString {
                field: "receiver name",
            })?),
            None => None,
        };
        let source_t = ffi::NDIlib_source_t {
            p_ndi_name: c_source.as_ptr(),
            __bindgen_anon_1: ffi::NDIlib_source_t__bindgen_ty_1 {
                p_url_address: std::ptr::null(),
            },
        };
        let create_t = ffi::NDIlib_recv_create_v3_t {
            source_to_connect_to: source_t,
            color_format: ffi::NDIlib_recv_color_format_UYVY_BGRA,
            bandwidth: ffi::NDIlib_recv_bandwidth_highest,
            allow_video_fields: false,
            p_ndi_recv_name: c_recv.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
        };
        // SAFETY: `v6.recv_create_v3` is the resolved create fn pointer. `create_t`
        // is fully initialised; its `p_ndi_name`/`p_ndi_recv_name` point at the
        // `c_source`/`c_recv` C strings kept alive across the call (the SDK copies
        // them). Returns an owned instance handle (or null), null-checked below;
        // nothing is dereferenced here.
        let instance = unsafe { (v6.recv_create_v3)(std::ptr::from_ref(&create_t)) };
        if instance.is_null() {
            return Err(NdiError::NullInstance {
                what: "NDI receiver",
            });
        }
        Ok(Self {
            table: v6,
            instance,
        })
    }

    /// Sample the next **video** frame, waiting at most `timeout_ms` for one.
    ///
    /// Returns `Some(frame)` if a video frame arrived, or `None` on the timeout or
    /// a non-video frame type (status-change / error / none) — the producer treats
    /// `None` as "no frame this tick" and never blocks. Audio and metadata are
    /// skipped (null sinks), so only video frames are ever allocated (and freed).
    ///
    /// # Errors
    /// Currently infallible at the FFI boundary (a fault surfaces as `None`); the
    /// `Result` is kept for forward compatibility with explicit disconnect signals.
    // reason: the FFI capture call — pass out-params the SDK fills (// SAFETY below).
    #[allow(unsafe_code)]
    pub fn capture_video(&self, timeout_ms: u32) -> Result<Option<RecvVideoFrame<'_>>, NdiError> {
        // Zeroed POD: the SDK overwrites it on a video frame; on any other type it
        // stays zero (p_data null → nothing to free).
        let mut video = MaybeUninit::<ffi::NDIlib_video_frame_v2_t>::zeroed();
        // SAFETY: `recv_capture_v3` is the resolved capture fn pointer; `self.instance`
        // is the live receiver. We pass a writable video out-param and NULL audio +
        // metadata sinks (so the SDK delivers only video and allocates nothing else).
        // It returns the frame type and, for a video frame, fills `video` with an
        // SDK-owned buffer we free via `RecvVideoFrame::drop`.
        let frame_type = unsafe {
            (self.table.recv_capture_v3)(
                self.instance,
                video.as_mut_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                timeout_ms,
            )
        };
        if frame_type == ffi::NDIlib_frame_type_video {
            // SAFETY: the SDK reported a video frame, so it fully initialised `video`.
            let frame = unsafe { video.assume_init() };
            Ok(Some(RecvVideoFrame {
                receiver: self,
                frame,
            }))
        } else {
            // none / status_change / error / audio / metadata: no video this sample.
            Ok(None)
        }
    }

    /// Sample the next **audio** frame (planar f32 / `FLTP`), waiting at most
    /// `timeout_ms`. Returns `Some` on an audio frame, `None` on timeout or a
    /// non-audio frame type. Video + metadata are skipped (null sinks), so only
    /// audio is ever allocated (and freed).
    ///
    /// # Errors
    /// Currently infallible at the FFI boundary (a fault surfaces as `None`); the
    /// `Result` mirrors [`Self::capture_video`] for forward compatibility.
    // reason: the FFI capture call — pass out-params the SDK fills (// SAFETY below).
    #[allow(unsafe_code)]
    pub fn capture_audio(&self, timeout_ms: u32) -> Result<Option<RecvAudioFrame<'_>>, NdiError> {
        let mut audio = MaybeUninit::<ffi::NDIlib_audio_frame_v3_t>::zeroed();
        // SAFETY: `recv_capture_v3` is the resolved capture fn pointer; `self.instance`
        // is the live receiver. We pass a NULL video sink, a writable audio out-param,
        // and a NULL metadata sink (so the SDK delivers only audio and allocates
        // nothing else). On an audio frame it fills `audio` with an SDK-owned buffer
        // we free via `RecvAudioFrame::drop`.
        let frame_type = unsafe {
            (self.table.recv_capture_v3)(
                self.instance,
                std::ptr::null_mut(),
                audio.as_mut_ptr(),
                std::ptr::null_mut(),
                timeout_ms,
            )
        };
        if frame_type == ffi::NDIlib_frame_type_audio {
            // SAFETY: the SDK reported an audio frame, so it fully initialised `audio`.
            let frame = unsafe { audio.assume_init() };
            Ok(Some(RecvAudioFrame {
                receiver: self,
                frame,
            }))
        } else {
            Ok(None)
        }
    }
}

impl std::fmt::Debug for NdiReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NdiReceiver")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl Drop for NdiReceiver {
    // reason: the FFI destroy call — release the SDK handle exactly once.
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.instance` is the live receiver from `create`, destroyed
        // exactly once here. Host-side teardown only, never on the hot path.
        unsafe {
            (self.table.recv_destroy)(self.instance);
        }
    }
}

/// An owned, SDK-allocated received video frame that frees on `Drop`.
///
/// Borrows its [`NdiReceiver`] (`&'r`) so the free target is alive by construction.
/// Copy the pixels out via [`RecvVideoFrame::data`] before it drops — once dropped,
/// the SDK memory is returned and the slice is gone.
pub struct RecvVideoFrame<'r> {
    receiver: &'r NdiReceiver,
    frame: ffi::NDIlib_video_frame_v2_t,
}

impl RecvVideoFrame<'_> {
    /// Frame width in pixels (the SDK `xres`), clamped to `>= 0`.
    #[must_use]
    pub fn width(&self) -> u32 {
        u32::try_from(self.frame.xres).unwrap_or(0)
    }

    /// Frame height in pixels (the SDK `yres`), clamped to `>= 0`.
    #[must_use]
    pub fn height(&self) -> u32 {
        u32::try_from(self.frame.yres).unwrap_or(0)
    }

    /// The line stride (bytes per row) of the received buffer.
    #[must_use]
    pub fn stride(&self) -> u32 {
        // SAFETY note: for video frames this union member is `line_stride_in_bytes`
        // (the `data_size_in_bytes` member is for compressed formats we do not use).
        #[allow(unsafe_code)]
        let stride = unsafe { self.frame.__bindgen_anon_1.line_stride_in_bytes };
        u32::try_from(stride).unwrap_or(0)
    }

    /// The classified packing of the received frame.
    #[must_use]
    pub fn fourcc(&self) -> RecvFourCc {
        RecvFourCc::classify(self.frame.FourCC)
    }

    /// The NDI per-frame timecode in 100 ns units (the producer's raw PTS).
    #[must_use]
    pub fn timecode(&self) -> i64 {
        self.frame.timecode
    }

    /// The received host pixel bytes (`stride * height` for the packed UYVY/BGRA
    /// formats this receiver requests). Empty if the SDK gave a null buffer.
    ///
    /// Borrows SDK-owned memory valid only until this frame drops — copy out before
    /// then.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        if self.frame.p_data.is_null() {
            return &[];
        }
        let len = u64::from(self.stride()).saturating_mul(u64::from(self.height()));
        let Ok(len) = usize::try_from(len) else {
            return &[];
        };
        // SAFETY: `p_data` is non-null SDK-owned memory holding the packed frame;
        // `len = stride * height` is exactly the byte count the SDK allocated for a
        // packed UYVY/BGRA frame. The slice borrows `self`, so it cannot outlive the
        // frame (and thus the SDK allocation).
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts(self.frame.p_data, len)
        }
    }
}

impl Drop for RecvVideoFrame<'_> {
    // reason: free the SDK-owned video buffer exactly once.
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.frame` is the SDK-allocated video frame from this receiver's
        // `capture_video`; we free it exactly once through the same (still-live, by
        // the `&'r` borrow) receiver instance. After this the buffer must not be
        // read — the borrow checker guarantees no `data()` slice outlives `self`.
        unsafe {
            (self.receiver.table.recv_free_video_v2)(
                self.receiver.instance,
                std::ptr::from_ref(&self.frame),
            );
        }
    }
}

impl std::fmt::Debug for RecvVideoFrame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecvVideoFrame")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("stride", &self.stride())
            .field("fourcc", &self.fourcc())
            .finish_non_exhaustive()
    }
}

/// An owned, SDK-allocated received **audio** frame (planar 32-bit float / `FLTP`)
/// that frees on `Drop`.
///
/// Like [`RecvVideoFrame`] it borrows its [`NdiReceiver`] (`&'r`) so the free
/// target is alive by construction. Copy the samples out via
/// [`RecvAudioFrame::data_bytes`] before it drops.
pub struct RecvAudioFrame<'r> {
    receiver: &'r NdiReceiver,
    frame: ffi::NDIlib_audio_frame_v3_t,
}

impl RecvAudioFrame<'_> {
    /// Sample rate in Hz (the SDK `sample_rate`), clamped to `>= 0`.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        u32::try_from(self.frame.sample_rate).unwrap_or(0)
    }

    /// Channel count (`no_channels`), clamped to `>= 0`.
    #[must_use]
    pub fn channels(&self) -> u32 {
        u32::try_from(self.frame.no_channels).unwrap_or(0)
    }

    /// Samples per channel (`no_samples`), clamped to `>= 0`.
    #[must_use]
    pub fn samples(&self) -> u32 {
        u32::try_from(self.frame.no_samples).unwrap_or(0)
    }

    /// Bytes between the start of each channel plane (`channel_stride_in_bytes`).
    #[must_use]
    pub fn channel_stride_bytes(&self) -> u32 {
        // SAFETY note: for audio frames this union member is `channel_stride_in_bytes`
        // (the `data_size_in_bytes` member is for compressed audio we do not use).
        #[allow(unsafe_code)]
        let stride = unsafe { self.frame.__bindgen_anon_1.channel_stride_in_bytes };
        u32::try_from(stride).unwrap_or(0)
    }

    /// The NDI per-frame timecode in 100 ns units.
    #[must_use]
    pub fn timecode(&self) -> i64 {
        self.frame.timecode
    }

    /// Whether the frame is planar 32-bit float (`FLTP`) — the only layout this
    /// receiver requests.
    #[must_use]
    pub fn is_fltp(&self) -> bool {
        self.frame.FourCC == ffi::NDIlib_FourCC_audio_type_FLTP
    }

    /// The received planar host bytes (`channels * channel_stride_bytes`). Empty if
    /// the SDK gave a null buffer. Borrows SDK-owned memory valid only until this
    /// frame drops — copy out before then. Read individual `f32` samples with
    /// `f32::from_ne_bytes` at `plane * channel_stride_bytes + sample * 4`.
    #[must_use]
    pub fn data_bytes(&self) -> &[u8] {
        if self.frame.p_data.is_null() {
            return &[];
        }
        let len = u64::from(self.channels()).saturating_mul(u64::from(self.channel_stride_bytes()));
        let Ok(len) = usize::try_from(len) else {
            return &[];
        };
        // SAFETY: `p_data` is non-null SDK-owned planar memory; `len = channels *
        // channel_stride` is the byte count the SDK allocated. The slice borrows
        // `self`, so it cannot outlive the frame (and thus the SDK allocation).
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts(self.frame.p_data, len)
        }
    }
}

impl Drop for RecvAudioFrame<'_> {
    // reason: free the SDK-owned audio buffer exactly once.
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `self.frame` is the SDK-allocated audio frame from this receiver's
        // `capture_audio`; we free it exactly once through the same (still-live, by
        // the `&'r` borrow) receiver instance. The borrow checker guarantees no
        // `data_bytes()` slice outlives `self`.
        unsafe {
            (self.receiver.table.recv_free_audio_v3)(
                self.receiver.instance,
                std::ptr::from_ref(&self.frame),
            );
        }
    }
}

impl std::fmt::Debug for RecvAudioFrame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecvAudioFrame")
            .field("sample_rate", &self.sample_rate())
            .field("channels", &self.channels())
            .field("samples", &self.samples())
            .finish_non_exhaustive()
    }
}
