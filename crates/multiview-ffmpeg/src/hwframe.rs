//! `AVHWFramesContext` / `AVHWDeviceContext` lifecycle scaffold (the `ffmpeg`
//! feature).
//!
//! This is the **only** module in the crate that touches raw libav FFI: every
//! other wrapper rides `ffmpeg_next`'s safe surface. The types here own an
//! `AVBufferRef`-counted hardware device / frames context and free it in `Drop`
//! via `av_buffer_unref`. [`HwDeviceContext`] is wired into the real decode and
//! encode paths: the decode stream creates one and calls
//! [`HwDeviceContext::attach_to_decoder`] (moving CUDA output frames to host via
//! [`transfer_hwframe_to_host`]), and the encoder attaches it via
//! [`HwDeviceContext::attach_to_encoder`]. [`HwFramesContext`] (the explicit
//! frames pool) is built and unit-tested but not yet bound in the decode path
//! (core-engine §7, §8.1, §12) — the remaining GPU-hwaccel scaffold.
//!
//! ## Safety contract (CLAUDE.md §7)
//! * Every `unsafe` block carries a `// SAFETY:` note stating the upheld
//!   invariant.
//! * Each handle owns exactly one ref on its `AVBufferRef` and releases it once
//!   in `Drop`; the pointer is never aliased out.
//! * The handles are `Send` (an owned ref-count may move threads) but **not**
//!   `Sync`: libav hardware contexts require external synchronization for shared
//!   access. They expose no interior mutability, so `!Sync` falls out of the raw
//!   pointer they hold (asserted in tests).
//! * No `extern "C"` callback is installed here; the `get_format`-style decoder
//!   callback that *will* live in the decode path must `catch_unwind` at the
//!   boundary and stay allocation-light (documented at its future call site).

// reason: this is the crate's designated raw-FFI module (CLAUDE.md §7). The
// workspace/crate lint is `unsafe_code = "deny"` (not `forbid`) precisely so
// the hardware-frame lifecycle can use raw libav FFI here; every `unsafe` block
// and `unsafe impl` below carries a `// SAFETY:` comment. No other module in
// the crate is allowed `unsafe`.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::ptr::NonNull;

use ffmpeg::ffi;
use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};
pub use crate::hwdecode::HwDeviceKind;

/// Resolve a [`HwDeviceKind`] to the libav `AVHWDeviceType` discriminant.
///
/// The pure enum + its `libav_name` live in [`crate::hwdecode`] (always
/// compiled, GPU-free); this is the libav-touching resolution, so it lives here
/// in the crate's designated FFI module.
///
/// # Errors
/// Returns [`FfmpegError::UnknownHwDevice`] if `kind` is
/// [`HwDeviceKind::Software`] (no libav hardware device) or the linked `FFmpeg`
/// build does not know the device type.
fn to_av_type(kind: HwDeviceKind) -> Result<ffi::AVHWDeviceType> {
    let name = kind.libav_name();
    // `Software` maps to the empty name — there is no hardware device to open.
    if name.is_empty() {
        return Err(FfmpegError::UnknownHwDevice(name.to_owned()));
    }
    let name_c = CString::new(name).map_err(|_| FfmpegError::UnknownHwDevice(name.to_owned()))?;
    // SAFETY: `av_hwdevice_find_type_by_name` reads a NUL-terminated C string
    // (guaranteed by `CString`) and returns a plain enum by value; it has no
    // ownership effects and cannot fail destructively. `name_c` outlives the
    // call.
    let ty = unsafe { ffi::av_hwdevice_find_type_by_name(name_c.as_ptr()) };
    if ty == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
        Err(FfmpegError::UnknownHwDevice(name.to_owned()))
    } else {
        Ok(ty)
    }
}

/// An owned reference to an `AVHWDeviceContext` (a GPU device handle).
///
/// Created with [`HwDeviceContext::create`]; the ref is released in `Drop`.
pub struct HwDeviceContext {
    /// Non-null `AVBufferRef*` owning one ref on the device context.
    ptr: NonNull<ffi::AVBufferRef>,
    kind: HwDeviceKind,
}

// SAFETY: the handle owns a heap `AVBufferRef` ref-count with no thread-affine
// interior state exposed; moving the owned ref between threads is sound. It is
// deliberately NOT `Sync` (no `unsafe impl Sync`): libav hardware contexts must
// be externally synchronized for shared access, and `*mut` is `!Sync` by
// default, so leaving `Sync` underived enforces that.
unsafe impl Send for HwDeviceContext {}

impl HwDeviceContext {
    /// Create a hardware device context for `kind`, optionally selecting a
    /// specific device (e.g. `"0"` / a DRM render node); `None` uses the
    /// default.
    ///
    /// This **requires a working GPU/driver at runtime**: with none present it
    /// returns a typed error instead of panicking, which is exactly how the
    /// GPU-free CI path exercises it.
    ///
    /// # Errors
    /// * [`FfmpegError::UnknownHwDevice`] — device type absent from this build.
    /// * [`FfmpegError::HwContext`] — libav could not create the device
    ///   (no driver / no device / bad selector).
    pub fn create(kind: HwDeviceKind, device: Option<&str>) -> Result<Self> {
        let ty = to_av_type(kind)?;
        let device_c = match device {
            Some(d) => {
                Some(CString::new(d).map_err(|_| FfmpegError::UnknownHwDevice(d.to_owned()))?)
            }
            None => None,
        };
        let device_ptr = device_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

        let mut raw: *mut ffi::AVBufferRef = std::ptr::null_mut();
        // SAFETY: `out` is a valid out-pointer to `raw` for the created buffer
        // ref; `ty` is a valid enum; `device_ptr` is either null or a
        // NUL-terminated string that outlives the call (`device_c` is still in
        // scope); opts/flags are null/0. On success libav writes a non-null
        // owning ref to `raw`; on failure it leaves it null and returns
        // negative. We take ownership of exactly that one ref below.
        let out = std::ptr::addr_of_mut!(raw);
        let rc =
            unsafe { ffi::av_hwdevice_ctx_create(out, ty, device_ptr, std::ptr::null_mut(), 0) };
        if rc < 0 {
            return Err(FfmpegError::HwContext(ffmpeg::Error::from(rc)));
        }
        let ptr = NonNull::new(raw).ok_or(FfmpegError::HwContext(ffmpeg::Error::Bug))?;
        Ok(Self { ptr, kind })
    }

    /// The device family this context targets.
    #[must_use]
    pub const fn kind(&self) -> HwDeviceKind {
        self.kind
    }

    /// Borrow the raw `AVBufferRef*` (for wiring into a decoder/frames context).
    ///
    /// The pointer stays owned by `self`; callers must not free it.
    #[must_use]
    pub fn as_raw(&self) -> *mut ffi::AVBufferRef {
        self.ptr.as_ptr()
    }

    /// Attach this device context to a not-yet-opened codec `context` (decoder
    /// **or** encoder) so the codec runs its hardware path on this GPU.
    ///
    /// Sets the context's `hw_device_ctx` to a **new owning reference** on this
    /// device buffer (via `av_buffer_ref`), which libav frees when the codec
    /// context is freed; `self` keeps its own ref. Must be called **before**
    /// `avcodec_open2` (i.e. before `Decoder::open`/`Video::open_as`), which is
    /// the only time libav reads `hw_device_ctx`.
    ///
    /// This is the shared raw-FFI body behind [`Self::attach_to_decoder`] and
    /// [`Self::attach_to_encoder`]; both reach the same not-yet-opened
    /// `codec::context::Context` (the encoder via its `DerefMut` chain).
    ///
    /// # Errors
    /// Returns [`FfmpegError::HwContext`] if libav cannot take a new reference on
    /// the device buffer (out of memory) — the caller then degrades to software
    /// / the default device.
    fn attach_to_codec_ctx(&self, context: &mut ffmpeg::codec::context::Context) -> Result<()> {
        // SAFETY: `self.ptr` is a live, non-null owning ref on the device
        // context (the `NonNull` invariant). `av_buffer_ref` returns a NEW
        // independent owning ref (or null on OOM) without consuming ours. We
        // hand that new ref to the codec context's `hw_device_ctx`, which libav
        // takes ownership of and unrefs on `avcodec_free_context`. `context` is
        // exclusively borrowed `&mut`, so writing the field is not aliased.
        // `as_mut_ptr` is `unsafe` only because it hands out the raw context
        // pointer; we use it solely to write the single scalar field below.
        unsafe {
            let new_ref = ffi::av_buffer_ref(self.ptr.as_ptr());
            if new_ref.is_null() {
                return Err(FfmpegError::HwContext(ffmpeg::Error::Bug));
            }
            let ctx_ptr = context.as_mut_ptr();
            (*ctx_ptr).hw_device_ctx = new_ref;
        }
        Ok(())
    }

    /// Attach this device context to a not-yet-opened decoder `context` so the
    /// decoder runs its hardware path on this GPU.
    ///
    /// A thin wrapper over [`Self::attach_to_codec_ctx`]; see it for the
    /// reference-counting + ordering contract. Must be called **before**
    /// `Decoder::open`/`video()`.
    ///
    /// # Errors
    /// Returns [`FfmpegError::HwContext`] if libav cannot take a new reference on
    /// the device buffer (out of memory) — the caller then degrades to software.
    pub fn attach_to_decoder(&self, context: &mut ffmpeg::codec::context::Context) -> Result<()> {
        self.attach_to_codec_ctx(context)
    }

    /// Attach this device context to a not-yet-opened video `encoder` so the
    /// encoder runs its hardware path (e.g. NVENC) on this GPU — the encode-side
    /// mirror of [`Self::attach_to_decoder`].
    ///
    /// `encoder::video::Video` derefs (`DerefMut`) through `encoder::Encoder`
    /// down to the underlying `codec::context::Context`, so writing
    /// `hw_device_ctx` on the deref'd `&mut Context` configures the *same*
    /// not-yet-opened context libav reads in `avcodec_open2`. Must be called
    /// **before**
    /// [`Video::open_as`](ffmpeg::codec::encoder::video::Video::open_as), which is
    /// the only time libav reads `hw_device_ctx`. Pins the chosen CUDA ordinal so
    /// encode lands on the admission-selected GPU instead of defaulting to
    /// ordinal 0 (Tier-2 P1a).
    ///
    /// # Errors
    /// Returns [`FfmpegError::HwContext`] if libav cannot take a new reference on
    /// the device buffer (out of memory) — the caller then degrades to a
    /// default-device open (never a panic).
    pub fn attach_to_encoder(
        &self,
        encoder: &mut ffmpeg::codec::encoder::video::Video,
    ) -> Result<()> {
        // Auto-deref coerces `&mut Video` through its `DerefMut` chain
        // (Video -> Encoder -> codec::context::Context) to the `&mut Context`
        // parameter — the same not-yet-opened context libav reads in `open_as`.
        self.attach_to_codec_ctx(encoder)
    }
}

/// Whether a decoded frame is a CUDA hardware surface (pixel format
/// [`Pixel::CUDA`]) — i.e. its pixels live in device memory and must be
/// downloaded with [`transfer_hwframe_to_host`] before the host pipeline can
/// read them.
///
/// The `*_cuvid` wrapper decoders normally output host NV12 directly (they do
/// the NVDEC decode + download internally), in which case this is `false` and no
/// transfer is needed; this predicate exists so the decode loop stays correct if
/// a future build is configured to return CUDA frames.
#[must_use]
pub fn is_cuda_frame(frame: &ffmpeg::frame::Video) -> bool {
    frame.format() == Pixel::CUDA
}

/// Download a hardware (e.g. CUDA/NVDEC) `src` frame into the host `dst` frame
/// via `av_hwframe_transfer_data` — the **budgeted CPU↔GPU copy** at the decode
/// boundary (core-engine §8.1). `dst` is left as a host frame in the surface
/// pool's software format (NV12 / P010), exactly what the rest of the pipeline
/// consumes (invariant #5).
///
/// On entry `dst` is reset to an empty frame so libav allocates a correctly
/// sized host buffer in the source's software format; on success its `pts` and
/// best-effort timestamp are copied across so downstream PTS handling is
/// unchanged.
///
/// # Errors
/// Returns [`FfmpegError::HwContext`] if the transfer fails (e.g. driver/device
/// error) — the caller logs once and degrades to software for that source, never
/// crashing the output (invariants #1/#2).
pub fn transfer_hwframe_to_host(
    src: &ffmpeg::frame::Video,
    dst: &mut ffmpeg::frame::Video,
) -> Result<()> {
    // A fresh destination so libav picks the surface pool's software format and
    // allocates a matching host buffer for the download.
    *dst = ffmpeg::frame::Video::empty();
    // SAFETY: `src` and `dst` are valid, distinct `AVFrame`s (owned by the
    // caller's bindings). `av_hwframe_transfer_data(dst, src, 0)` reads the GPU
    // surface referenced by `src->hw_frames_ctx` and writes host planes into
    // `dst`, allocating `dst`'s buffer; it does not take ownership of either
    // frame. The `&mut`/`&` borrows guarantee `dst != src` and exclusive write
    // access to `dst`. A negative return is a recoverable error surfaced below.
    let rc = unsafe { ffi::av_hwframe_transfer_data(dst.as_mut_ptr(), src.as_ptr(), 0) };
    if rc < 0 {
        return Err(FfmpegError::HwContext(ffmpeg::Error::from(rc)));
    }
    // Carry the timestamps across the download so the decode loop's PTS handling
    // sees the same values it would from a software frame.
    dst.set_pts(src.pts());
    // SAFETY: both pointers are valid `AVFrame`s for the duration of the call;
    // `best_effort_timestamp` is a plain scalar field copy, no ownership effects.
    unsafe {
        (*dst.as_mut_ptr()).best_effort_timestamp = (*src.as_ptr()).best_effort_timestamp;
    }
    Ok(())
}

impl Drop for HwDeviceContext {
    fn drop(&mut self) {
        let mut raw = self.ptr.as_ptr();
        // SAFETY: `raw` is the single owning ref this handle holds (non-null by
        // the `NonNull` invariant). `av_buffer_unref` takes the address of
        // `raw`, releases exactly one ref, and nulls the local pointer; we never
        // touch `self.ptr` again. This is a synchronous free — never run inside
        // a Tokio async destructor (CLAUDE.md §7); callers drop hardware handles
        // on their data-plane thread.
        unsafe {
            ffi::av_buffer_unref(std::ptr::addr_of_mut!(raw));
        }
    }
}

/// Geometry + format of a hardware frame pool to allocate.
#[derive(Debug, Clone, Copy)]
pub struct HwFramesSpec {
    /// The hardware pixel format (e.g. [`Pixel::CUDA`], [`Pixel::VAAPI`]).
    pub hw_format: Pixel,
    /// The software pixel format the surfaces decode to / are read back as
    /// (e.g. [`Pixel::NV12`], [`Pixel::P010LE`]).
    pub sw_format: Pixel,
    /// Surface width.
    pub width: u32,
    /// Surface height.
    pub height: u32,
    /// Initial pool size (surfaces pre-allocated); `0` lets libav choose.
    pub initial_pool_size: u32,
}

/// An owned reference to an `AVHWFramesContext` (a pool of GPU surfaces) tied to
/// a device context.
///
/// Created with [`HwFramesContext::create`]; the ref is released in `Drop`.
pub struct HwFramesContext {
    ptr: NonNull<ffi::AVBufferRef>,
    spec: HwFramesSpec,
}

// SAFETY: same contract as `HwDeviceContext` — owns one `AVBufferRef` ref,
// movable across threads, intentionally `!Sync`.
unsafe impl Send for HwFramesContext {}

impl HwFramesContext {
    /// Allocate and initialize a hardware frames context (surface pool) on
    /// `device` with the given `spec`.
    ///
    /// # Errors
    /// * [`FfmpegError::HwContext`] — allocation or initialization failed
    ///   (e.g. the device cannot back the requested format/size).
    pub fn create(device: &HwDeviceContext, spec: HwFramesSpec) -> Result<Self> {
        // SAFETY: `device.as_raw()` is a live, non-null `AVBufferRef*` owned by
        // `device` which outlives this call. `av_hwframe_ctx_alloc` returns a
        // new owning ref (or null) derived from it; it does not consume the
        // device ref.
        let raw = unsafe { ffi::av_hwframe_ctx_alloc(device.as_raw()) };
        let ptr = NonNull::new(raw).ok_or(FfmpegError::HwContext(ffmpeg::Error::Bug))?;

        // SAFETY: `ptr` is the just-allocated, non-null frames-context buffer.
        // Its `.data` is an `AVHWFramesContext*` (libav guarantees this for a
        // buffer from `av_hwframe_ctx_alloc`, and that the storage is correctly
        // aligned for the struct). We write only scalar configuration fields
        // before init; no aliasing, the pointer is exclusively ours here.
        // reason (cast_ptr_alignment): libav allocates `data` aligned for
        // `AVHWFramesContext`; the `*mut u8` field type is just how the C ABI
        // exposes an opaque pointer.
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            let frames = ptr.as_ptr();
            let ctx = (*frames).data.cast::<ffi::AVHWFramesContext>();
            (*ctx).format = pixel_to_av(spec.hw_format);
            (*ctx).sw_format = pixel_to_av(spec.sw_format);
            (*ctx).width = i32::try_from(spec.width).unwrap_or(i32::MAX);
            (*ctx).height = i32::try_from(spec.height).unwrap_or(i32::MAX);
            (*ctx).initial_pool_size = i32::try_from(spec.initial_pool_size).unwrap_or(i32::MAX);
        }

        // SAFETY: `ptr` is a valid, configured frames-context ref. `av_hwframe_ctx_init`
        // finalizes the pool and returns negative on failure without taking
        // ownership of our ref; on failure we still own `ptr` and free it via the
        // `Drop` of the temporary handle constructed below.
        let rc = unsafe { ffi::av_hwframe_ctx_init(ptr.as_ptr()) };
        let handle = Self { ptr, spec };
        if rc < 0 {
            // `handle`'s `Drop` releases the ref; surface the typed error.
            drop(handle);
            return Err(FfmpegError::HwContext(ffmpeg::Error::from(rc)));
        }
        Ok(handle)
    }

    /// The spec this pool was created with.
    #[must_use]
    pub const fn spec(&self) -> HwFramesSpec {
        self.spec
    }

    /// Borrow the raw `AVBufferRef*` (for wiring into a decoder's
    /// `hw_frames_ctx`). Stays owned by `self`.
    #[must_use]
    pub fn as_raw(&self) -> *mut ffi::AVBufferRef {
        self.ptr.as_ptr()
    }
}

impl Drop for HwFramesContext {
    fn drop(&mut self) {
        let mut raw = self.ptr.as_ptr();
        // SAFETY: `raw` is the single owning ref this handle holds (non-null).
        // `av_buffer_unref` takes the address of `raw`, releases exactly one
        // ref, and nulls the local; we never reuse `self.ptr`. Synchronous
        // free — not inside an async destructor (CLAUDE.md §7).
        unsafe {
            ffi::av_buffer_unref(std::ptr::addr_of_mut!(raw));
        }
    }
}

/// Convert an `ffmpeg_next` [`Pixel`] into the raw `AVPixelFormat` enum the FFI
/// fields expect. This goes through `ffmpeg_next`'s own `From` impl, so it
/// stays correct across versions.
fn pixel_to_av(format: Pixel) -> ffi::AVPixelFormat {
    format.into()
}
