//! Local DRM/KMS display output (DEV-B1, [ADR-0044]) — the `display` raw-frame
//! sink: HDMI/DisplayPort glass driven directly from KMS atomic commits, with
//! no X11/Wayland anywhere.
//!
//! ## Architecture (brief: `docs/research/display-out.md` §0–§1)
//!
//! The display sink is a **raw-frame sink, never a `PacketMuxSink`**: it
//! consumes the pre-encode NV12 canvas through a wait-free single-slot
//! latest-frame [`mailbox`](frame_mailbox) (the preview-tap isolation shape
//! over `multiview-framestore`'s `LatestSlot`), so the engine publishes each
//! tick's canvas with one wait-free overwrite and never awaits the sink
//! (invariants #1 + #10). A **dedicated thread owns the DRM fd** and runs the
//! page-flip-event-driven loop:
//!
//! 1. wait (bounded poll) on the DRM event fd;
//! 2. on page-flip-complete → take the **latest** mailbox frame →
//!    `atomic_commit(NONBLOCK | PAGE_FLIP_EVENT)`;
//! 3. the kernel enforces **at most one in-flight commit per CRTC** — a second
//!    nonblocking commit fails `EBUSY`, which *is* mailbox conflation (never
//!    queue, never spin-retry; the next flip event drains the latest frame);
//! 4. no new frame ⇒ **no commit** — KMS repeats the current framebuffer for
//!    free;
//! 5. `TEST_ONLY` validates the full configuration at startup, **before** the
//!    one `ALLOW_MODESET` commit; `ALLOW_MODESET` is never on the frame path.
//!
//! Everything in this module except [`kms`] is **pure Rust, always compiled,
//! and CI-tested without hardware**: the mailbox, the mode-selection policy
//! (EDID preferred + exact-rational refresh match + CVT-RB forced modes), the
//! [`FlipDriver`] EBUSY/conflation state machine, the NV12→XRGB software
//! conversion, and the sink thread itself, which is generic over the
//! [`KmsBackend`] trait seam. The real ioctl-speaking backend lives in
//! [`kms`] behind the off-by-default `display-kms` feature and is exercised
//! only on hardware.
//!
//! ## Phase-0 spike verdict — fences (`IN_FENCE_FD` / `OUT_FENCE_PTR`)
//!
//! Verified against the `drm` crate **0.15.0** source (2026-06-10):
//!
//! * `AtomicCommitFlags` carries all four required flags (`NONBLOCK`,
//!   `PAGE_FLIP_EVENT`, `TEST_ONLY`, `ALLOW_MODESET`), and
//!   `atomic_commit(flags, AtomicModeReq)` takes arbitrary
//!   property/value pairs.
//! * Both fence properties **are expressible** through the generic property
//!   path: `property::Value::SignedRange(i64)` carries `IN_FENCE_FD` (a
//!   signed-range fd property) and the raw `property::Value::Unknown(u64)`
//!   escape hatch can carry `OUT_FENCE_PTR` (a signed-range property whose
//!   value is a **user-space pointer** the kernel writes an `s32` sync-file fd
//!   through). drm-rs has no first-class helpers, but nothing is missing.
//! * **Verdict: flip-event-only for v1 — deliberately.** `OUT_FENCE_PTR`
//!   needs raw-pointer→integer plumbing plus kernel write-back through that
//!   pointer, colliding with this workspace's `as_conversions`-deny /
//!   no-`unsafe` policy for zero v1 benefit: the v1 producer is the **CPU**
//!   (the scanout buffer is fully written on the sink thread *before* the
//!   commit ioctl is issued, so there is no GPU producer for `IN_FENCE_FD` to
//!   order), and presentation-skew telemetry already comes from the
//!   `PageFlipEvent` timestamps the kernel delivers per flip. Fences become
//!   load-bearing only with DEV-B3's GPU render path; the verified fallback
//!   there is the full DRM **syncobj** suite, which drm 0.15 exposes
//!   (`create_syncobj`, `syncobj_wait`, timeline variants, fd conversion).
//!
//! ## Phase-0 spike verdict — `gbm` scanout allocation
//!
//! Verified against the `gbm` crate **0.18.0** source: the buffer-object API
//! **suffices** for XRGB scanout allocation —
//! `Device::create_buffer_object::<()>(w, h, Format::Xrgb8888,
//! SCANOUT | WRITE | LINEAR)` plus `map_mut`/`write`, `stride()`, `handle()`
//! and `modifier()` provide everything `ADDFB2` needs. One caveat drove the
//! integration shape: gbm 0.18's optional `drm-support` interop targets drm
//! **0.14**, not 0.15, so this crate uses gbm *without* that feature and
//! adapts `(handle, pitch, size, format)` onto drm 0.15's `buffer::Buffer`
//! traits itself (`buffer::Handle` is publicly constructible from a
//! `NonZeroU32`). The real backend allocates GBM scanout BOs first and falls
//! back to KMS **dumb buffers** where GBM scanout allocation is unavailable
//! (NVIDIA's proprietary driver documents GBM buffer submission to its KMS as
//! unsupported; dumb buffers are the universal KMS CPU-scanout primitive).
//!
//! [ADR-0044]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0044.md

pub mod canvas;
pub mod device;
pub mod flip;
pub mod mailbox;
pub mod mode;
pub mod sink;
pub mod strategy;

/// The real drm-rs + gbm KMS backend (feature `display-kms`). Everything that
/// touches an ioctl lives here; its hardware paths are exercised only on
/// hardware (CI covers the pure seam above through the scripted mock).
#[cfg(feature = "display-kms")]
pub mod kms;

pub use canvas::{nv12_to_xrgb, CanvasError, DisplayCanvas, DmabufImage, DmabufPlane};
pub use device::{
    ConnectorDesc, ConnectorSelector, DisplayError, FlipEvent, HeadSetup, KmsBackend, SubmitError,
};
pub use flip::FlipDriver;
pub use mailbox::{frame_mailbox, FramePublisher, FrameReader, MailboxFrame};
pub use mode::{
    cvt_rb_mode, refresh_matches, select_mode, DisplayModeInfo, ForcedMode, ModeError, ModeRequest,
    SelectedMode,
};
pub use sink::{DisplaySink, DisplaySinkConfig, DisplaySinkHandle, DisplayStats, StatsSnapshot};
pub use strategy::{
    parse_in_formats_blob, plane_supports_nv12, select_buffer_strategy, BufferStrategy,
    CanvasDelivery, DrmFormat, PlaneFormatCaps, ScanoutCaps,
};
