//! Checked `AVFrame` allocation and plane access (#391 PB-1/PB-2).
//!
//! `ffmpeg_next`'s [`Video::new`] ignores the `av_frame_get_buffer` result,
//! and [`Video::data`]/[`Video::stride`] turn the signed `linesize` into a
//! `usize` and build one `linesize * plane_height` slice. Both are unsound
//! on valid-but-unusual input: a failed allocation leaves no planes (a later
//! `stride(0)` panics), and a negative linesize (a bottom-up frame, as
//! libavfilter's `vflip` produces) becomes a huge slice length — undefined
//! behaviour before any byte is read.
//!
//! * [`alloc_video`] allocates a frame and checks the dimensions, the pixel
//!   format and the allocation result before returning it, so a frame it
//!   hands back always has every plane of its format.
//! * [`plane_rows`] validates a plane's layout — plane index, signed stride,
//!   plane height from the format's chroma subsampling, and that every row's
//!   span lies inside one of the frame's own reference-counted buffers — and
//!   only then yields the rows one at a time, in display order, whatever the
//!   stride's sign. No slice longer than one row is ever constructed.
//! * [`check_swscale_source`] / [`check_swscale_destination`] add the
//!   libswscale contract on top: `libavutil/frame.h` documents that
//!   "swscale can read up to 16 bytes beyond the planes", so
//!   [`SWSCALE_PADDING`] bytes past the plane's highest active byte must lie
//!   in the same owning buffer. Rust row reads and `av_frame_copy` read only
//!   active bytes and keep the active-only check.

use core::marker::PhantomData;

use ffmpeg::ffi;
use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};

/// Largest width or height [`alloc_video`] accepts. 16384 covers every
/// production raster (8K UHD is 7680 x 4320) with headroom, and keeps a
/// corrupt or hostile stream from requesting a multi-gigabyte frame.
pub const MAX_FRAME_DIMENSION: u32 = 16_384;

/// Allocate a `format` frame of `width x height` with refcounted buffers.
///
/// # Errors
/// [`FfmpegError::FrameLayout`] for a zero or over-[`MAX_FRAME_DIMENSION`]
/// size, a hardware/unknown pixel format or a size libav rejects;
/// [`FfmpegError::FrameAlloc`] when `av_frame_alloc`/`av_frame_get_buffer`
/// fail. The frame is never returned half-built.
pub fn alloc_video(format: Pixel, width: u32, height: u32) -> Result<Video> {
    if width == 0 || height == 0 {
        return Err(FfmpegError::FrameLayout(
            "frame dimensions must be non-zero",
        ));
    }
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(FfmpegError::FrameLayout(
            "frame dimensions exceed MAX_FRAME_DIMENSION",
        ));
    }
    let av_format = ffi::AVPixelFormat::from(format);
    let flags =
        descriptor_flags(av_format).ok_or(FfmpegError::FrameLayout("unknown pixel format"))?;
    let hwaccel = u64::try_from(ffi::AV_PIX_FMT_FLAG_HWACCEL).unwrap_or(u64::MAX);
    if flags & hwaccel != 0 {
        return Err(FfmpegError::FrameLayout(
            "hardware pixel formats have no CPU planes",
        ));
    }
    let (w, h) = (
        libc::c_int::try_from(width)
            .map_err(|_| FfmpegError::FrameLayout("frame width overflows"))?,
        libc::c_int::try_from(height)
            .map_err(|_| FfmpegError::FrameLayout("frame height overflows"))?,
    );
    #[allow(unsafe_code)]
    // SAFETY: `av_image_check_size` only reads its integer arguments; a null
    // log context is documented as allowed.
    let size_ok = unsafe { ffi::av_image_check_size(width, height, 0, core::ptr::null_mut()) };
    if size_ok < 0 {
        return Err(FfmpegError::FrameLayout("libav rejects the frame size"));
    }
    let mut frame = new_frame();
    #[allow(unsafe_code)]
    // SAFETY: `as_mut_ptr` returns the frame's own `AVFrame` pointer (from
    // `av_frame_alloc`); it is checked for null before any dereference, and
    // `av_frame_get_buffer` is called on a frame whose format/width/height are
    // set and that holds no buffers yet, as its contract requires.
    let ret = unsafe {
        let raw = frame.as_mut_ptr();
        if raw.is_null() {
            return Err(FfmpegError::FrameAlloc("av_frame_alloc returned null"));
        }
        (*raw).format = pixel_format_code(av_format);
        (*raw).width = w;
        (*raw).height = h;
        ffi::av_frame_get_buffer(raw, 0)
    };
    if ret < 0 {
        return Err(FfmpegError::FrameAlloc("av_frame_get_buffer failed"));
    }
    #[allow(unsafe_code)]
    // SAFETY: `av_pix_fmt_count_planes` only reads the static format table.
    let expected = unsafe { ffi::av_pix_fmt_count_planes(av_format) };
    let expected = usize::try_from(expected).unwrap_or(0);
    if expected == 0 || frame.planes() < expected {
        return Err(FfmpegError::FrameAlloc("allocated frame is missing planes"));
    }
    for plane in 0..expected {
        #[allow(unsafe_code)]
        // SAFETY: `plane < expected <= 8 == AV_NUM_DATA_POINTERS`; the frame
        // pointer is non-null (checked above) and owned by `frame`.
        let (data, linesize) = unsafe {
            let raw = frame.as_ptr();
            (
                (*raw).data.get(plane).copied(),
                (*raw).linesize.get(plane).copied(),
            )
        };
        let (Some(data), Some(linesize)) = (data, linesize) else {
            return Err(FfmpegError::FrameAlloc("allocated plane is unusable"));
        };
        if data.is_null() || linesize <= 0 {
            return Err(FfmpegError::FrameAlloc("allocated plane is unusable"));
        }
    }
    Ok(frame)
}

#[cfg(test)]
std::thread_local! {
    /// Test fault seam: when set, the next [`new_frame`] on this thread
    /// behaves as if `av_frame_alloc` returned null (out of memory).
    static FAIL_NEXT_FRAME_ALLOC: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Test fault seam: make the next frame allocation on this thread fail as
/// if `av_frame_alloc` returned null.
#[cfg(test)]
pub(crate) fn fail_next_frame_alloc() {
    FAIL_NEXT_FRAME_ALLOC.with(|f| f.set(true));
}

/// `Video::empty()` (`av_frame_alloc`), whose pointer may be null; every
/// caller null-checks it before any accessor runs.
fn new_frame() -> Video {
    #[cfg(test)]
    if FAIL_NEXT_FRAME_ALLOC.with(|f| f.replace(false)) {
        #[allow(unsafe_code)]
        // SAFETY: a non-owning wrapper around null is exactly the state
        // `Video::empty()` produces when `av_frame_alloc` fails; its `Drop`
        // frees nothing, and `alloc_video` rejects it before any access.
        return unsafe { Video::wrap(core::ptr::null_mut()) };
    }
    Video::empty()
}

/// A privately owned, checked CPU frame for reuse across conversions
/// ([`crate::Scaler::run_into`], the media player's alpha scratch, #391).
///
/// Only [`ScratchFrame::alloc`] creates one (through [`alloc_video`], so the
/// `AVFrame` is non-null and every plane is allocated), and the inner frame
/// is never handed out mutably, so its format, size and buffers cannot be
/// changed behind the wrapper (e.g. raising the height with the safe
/// `Video::set_height` over a small allocation). Plane access still
/// re-validates the span against the frame's own buffers
/// ([`ScratchFrame::plane`], [`ScratchFrame::plane_mut`]); writes also
/// require the buffers to be writable (`av_frame_is_writable`).
pub struct ScratchFrame {
    frame: Video,
}

impl core::fmt::Debug for ScratchFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ScratchFrame")
            .field("format", &self.format())
            .field("width", &self.width())
            .field("height", &self.height())
            .finish()
    }
}

impl ScratchFrame {
    /// Allocate a checked `format` frame of `width x height`.
    ///
    /// # Errors
    /// As [`alloc_video`].
    pub fn alloc(format: Pixel, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            frame: alloc_video(format, width, height)?,
        })
    }

    /// The frame, read-only (for `plane_rows`, PTS, reading the geometry).
    #[must_use]
    pub const fn frame(&self) -> &Video {
        &self.frame
    }

    /// Pixel format.
    #[must_use]
    pub fn format(&self) -> Pixel {
        self.frame.format()
    }

    /// Width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.frame.width()
    }

    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.frame.height()
    }

    /// Whether this frame is a `format` frame of `width x height`.
    #[must_use]
    pub fn matches(&self, format: Pixel, width: u32, height: u32) -> bool {
        self.format() == format && self.width() == width && self.height() == height
    }

    /// `plane` as one validated forward span plus its stride: the slice
    /// covers rows `0..plane_height`, the last one only up to the row's
    /// active bytes, and lies inside one of the frame's buffers.
    ///
    /// # Errors
    /// [`FfmpegError::FrameLayout`] when the plane fails validation.
    pub fn plane(&self, plane: usize) -> Result<(&[u8], usize)> {
        let (ptr, len, stride) = writable_span(&self.frame, plane, 0)?;
        #[allow(unsafe_code)]
        // SAFETY: `writable_span` proved `[ptr, ptr + len)` lies inside one
        // `AVBufferRef` the frame holds (initialized libav memory); the
        // shared borrow of `self` keeps the frame alive and unmutated.
        let bytes = unsafe { core::slice::from_raw_parts(ptr.cast_const(), len) };
        Ok((bytes, stride))
    }

    /// `plane` as one validated, writable forward span plus its stride (see
    /// [`Self::plane`]).
    ///
    /// # Errors
    /// [`FfmpegError::FrameLayout`] when the plane fails validation or the
    /// buffers are shared (not writable).
    pub fn plane_mut(&mut self, plane: usize) -> Result<(&mut [u8], usize)> {
        self.check_writable()?;
        let (ptr, len, stride) = writable_span(&self.frame, plane, 0)?;
        #[allow(unsafe_code)]
        // SAFETY: as `plane`, and the buffers are exclusively ours
        // (`av_frame_is_writable`, checked above); `&mut self` makes this
        // the only live view of them.
        let bytes = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        Ok((bytes, stride))
    }

    /// Validate every plane of the format as a writable destination for a
    /// full-frame write (libswscale): positive linesize, every row's active
    /// bytes inside the frame's own buffers, buffers not shared.
    ///
    /// # Errors
    /// [`FfmpegError::FrameLayout`] naming the first failure.
    pub fn check_destination(&mut self) -> Result<()> {
        self.check_writable()?;
        let planes = format_planes(self.frame.format())?;
        for plane in 0..planes {
            writable_span(&self.frame, plane, 0)?;
        }
        Ok(())
    }

    /// [`Self::check_destination`] for a `sws_scale` write: additionally
    /// [`SWSCALE_PADDING`] bytes past every plane's last active byte must
    /// lie in the same owning buffer ([`check_swscale_destination`]).
    ///
    /// # Errors
    /// [`FfmpegError::FrameLayout`] naming the first failure.
    pub fn check_swscale_destination(&mut self) -> Result<()> {
        self.check_writable()?;
        check_swscale_destination(&self.frame)
    }

    /// The raw frame, for a libav call that writes the already-validated
    /// planes ([`Self::check_destination`]) without changing the frame's
    /// format, size or buffers.
    pub(crate) fn video_mut(&mut self) -> &mut Video {
        &mut self.frame
    }

    fn check_writable(&mut self) -> Result<()> {
        #[allow(unsafe_code)]
        // SAFETY: the pointer is this frame's own `AVFrame`, null-checked;
        // `av_frame_is_writable` only reads the buffer refcounts.
        let writable = unsafe {
            let raw = self.frame.as_mut_ptr();
            !raw.is_null() && ffi::av_frame_is_writable(raw) > 0
        };
        if writable {
            Ok(())
        } else {
            Err(FfmpegError::FrameLayout("frame buffers are not writable"))
        }
    }

    /// Test seam: wrap an arbitrary frame, bypassing [`Self::alloc`], to
    /// prove the plane validation catches a malformed one.
    #[cfg(test)]
    pub(crate) fn from_video_unchecked(frame: Video) -> Self {
        Self { frame }
    }
}

/// The number of planes of `format`.
fn format_planes(format: Pixel) -> Result<usize> {
    #[allow(unsafe_code)]
    // SAFETY: `av_pix_fmt_count_planes` only reads the static format table.
    let planes = unsafe { ffi::av_pix_fmt_count_planes(ffi::AVPixelFormat::from(format)) };
    match usize::try_from(planes) {
        Ok(n) if (1..=8).contains(&n) => Ok(n),
        _ => Err(FfmpegError::FrameLayout("unknown pixel format")),
    }
}

/// Active bytes of one row of `plane` of `frame` (`av_image_get_linesize`
/// for the frame's format and width).
fn row_bytes(frame: &Video, plane: usize) -> Result<usize> {
    let width = libc::c_int::try_from(frame.width())
        .map_err(|_| FfmpegError::FrameLayout("frame width overflows"))?;
    let index =
        libc::c_int::try_from(plane).map_err(|_| FfmpegError::FrameLayout("plane index"))?;
    #[allow(unsafe_code)]
    // SAFETY: `av_image_get_linesize` reads the static format table; a
    // negative result (invalid plane/width/format) is rejected below.
    let bytes = unsafe {
        ffi::av_image_get_linesize(ffi::AVPixelFormat::from(frame.format()), width, index)
    };
    usize::try_from(bytes).map_err(|_| FfmpegError::FrameLayout("plane row size"))
}

/// Size in bytes of the palette libav keeps in `data[1]` of a paletted
/// (`AV_PIX_FMT_FLAG_PAL`) frame: 256 `u32` entries.
const PALETTE_BYTES: usize = 256 * 4;

/// Validate every plane of `frame` as a full-frame **source** for a libav
/// read (`sws_scale`, `av_frame_copy`) or a row copy: for each plane of the
/// format, the row size from `av_image_get_linesize` (format + width), the
/// plane height with chroma subsampling applied and the signed linesize
/// must put every row inside one of the frame's own reference-counted
/// buffers ([`plane_rows`]); a paletted format's `data[1]` palette must lie
/// in one too. Either linesize sign is accepted (libswscale and
/// `av_image_copy` read bottom-up planes). A frame with no refcounted
/// buffers (nothing to bound it against), zero dimensions or a
/// hardware/unknown format is refused.
///
/// # Errors
/// [`FfmpegError::FrameLayout`] naming the first failure.
pub fn check_source(frame: &Video) -> Result<()> {
    source_planes(frame, 0)
}

/// Bytes libswscale may read (and write) past a plane: `libavutil/frame.h`
/// on `AVFrame::data` — "Some filters and swscale can read up to 16 bytes
/// beyond the planes, if these filters are to be used, then 16 extra bytes
/// must be allocated." `av_frame_get_buffer` (hence [`alloc_video`]) adds
/// at least this much after every plane.
pub const SWSCALE_PADDING: usize = 16;

/// [`check_source`] for a `sws_scale` read: additionally, for every plane,
/// the [`SWSCALE_PADDING`] bytes after the plane's highest active byte must
/// lie in the same owning buffer as its rows. "Beyond" is in address order
/// (libswscale's SIMD row loads run past a row's end): for a forward plane
/// that is after the last row, for a bottom-up (negative linesize) plane
/// after row 0, whose end is the plane's highest address. Nothing is
/// required before the lowest address: libswscale never reads before a
/// row's first byte, and a flipped `av_frame_get_buffer` plane 0 starts at
/// its buffer's first byte, so such a requirement would refuse every
/// `vflip` frame.
///
/// # Errors
/// [`FfmpegError::FrameLayout`] naming the first failure.
pub fn check_swscale_source(frame: &Video) -> Result<()> {
    source_planes(frame, SWSCALE_PADDING)
}

/// Validate every plane of `frame` as a `sws_scale` **destination**: every
/// row's active bytes plus [`SWSCALE_PADDING`] bytes after the plane's
/// highest active byte inside one owning buffer, and a forward (positive)
/// linesize. Does not check writability (see
/// [`ScratchFrame::check_swscale_destination`]).
///
/// # Errors
/// [`FfmpegError::FrameLayout`] naming the first failure.
pub fn check_swscale_destination(frame: &Video) -> Result<()> {
    let planes = format_planes(frame.format())?;
    for plane in 0..planes {
        writable_span(frame, plane, SWSCALE_PADDING)?;
    }
    Ok(())
}

/// [`check_source`] with `pad` readable bytes required past every plane's
/// highest active byte.
fn source_planes(frame: &Video, pad: usize) -> Result<()> {
    let planes = format_planes(frame.format())?;
    if frame.width() == 0 || frame.height() == 0 {
        return Err(FfmpegError::FrameLayout(
            "frame dimensions must be non-zero",
        ));
    }
    for plane in 0..planes {
        let bytes = row_bytes(frame, plane)?;
        let rows = padded_plane_rows(frame, plane, bytes, pad)?;
        if rows.rows == 0 {
            return Err(FfmpegError::FrameLayout("plane is empty"));
        }
    }
    let av_format = ffi::AVPixelFormat::from(frame.format());
    let flags =
        descriptor_flags(av_format).ok_or(FfmpegError::FrameLayout("unknown pixel format"))?;
    let pal = u64::try_from(ffi::AV_PIX_FMT_FLAG_PAL).unwrap_or(u64::MAX);
    if flags & pal != 0 {
        #[allow(unsafe_code)]
        // SAFETY: the pointer is `frame`'s own `AVFrame` (non-null:
        // `plane_rows` above already refused a null frame); `data` has
        // `AV_NUM_DATA_POINTERS` entries, so index 1 exists.
        let (raw, palette) = unsafe {
            let raw = frame.as_ptr();
            (raw, (*raw).data[1])
        };
        if palette.is_null() {
            return Err(FfmpegError::FrameLayout("paletted frame has no palette"));
        }
        let low = palette.expose_provenance();
        let high = low
            .checked_add(PALETTE_BYTES)
            .ok_or(FfmpegError::FrameLayout("palette span overflows"))?;
        if !span_in_buffers(raw, low, high) {
            return Err(FfmpegError::FrameLayout(
                "palette lies outside the frame's buffers",
            ));
        }
    }
    Ok(())
}

/// Whether the address range `[low, high)` lies inside one buffer `raw`
/// holds a reference to.
fn span_in_buffers(raw: *const ffi::AVFrame, low: usize, high: usize) -> bool {
    owned_buffers(raw).any(|(start, size)| {
        start
            .checked_add(size)
            .is_some_and(|end| low >= start && high <= end)
    })
}

/// `(first byte, span length, stride)` of `plane` of `frame`, a forward
/// (positive linesize) plane whose every row's active bytes
/// (`av_image_get_linesize`), plus `pad` bytes after the last one, lie in
/// one buffer the frame owns — checked by [`padded_plane_rows`] — and whose
/// span `stride * (rows - 1) + row_bytes` therefore lies in that same
/// contiguous buffer. The returned span never includes the padding.
fn writable_span(frame: &Video, plane: usize, pad: usize) -> Result<(*mut u8, usize, usize)> {
    let planes = format_planes(frame.format())?;
    if plane >= planes {
        return Err(FfmpegError::FrameLayout("plane is not part of the format"));
    }
    let row_bytes = row_bytes(frame, plane)?;
    let rows = padded_plane_rows(frame, plane, row_bytes, pad)?;
    if rows.rows == 0 || row_bytes == 0 {
        return Err(FfmpegError::FrameLayout("plane is empty"));
    }
    let stride = usize::try_from(rows.linesize)
        .map_err(|_| FfmpegError::FrameLayout("plane is not laid out top-down"))?;
    let len = stride
        .checked_mul(rows.rows - 1)
        .and_then(|v| v.checked_add(row_bytes))
        .ok_or(FfmpegError::FrameLayout("plane span overflows"))?;
    Ok((rows.first.cast_mut(), len, stride))
}

/// The `AVFrame::format` code of `format` (an `AVPixelFormat` discriminant).
fn pixel_format_code(format: ffi::AVPixelFormat) -> libc::c_int {
    // reason: `AVPixelFormat` is a `#[repr(i32)]` C enum; its discriminant is
    // exactly the integer libav stores in `AVFrame::format`.
    #[allow(clippy::as_conversions)]
    let code = format as libc::c_int;
    code
}

fn descriptor_flags(format: ffi::AVPixelFormat) -> Option<u64> {
    #[allow(unsafe_code)]
    // SAFETY: `av_pix_fmt_desc_get` returns a pointer into libavutil's
    // static, immutable descriptor table, or null for an unknown format; it
    // is checked for null before the read.
    unsafe {
        let desc = ffi::av_pix_fmt_desc_get(format);
        if desc.is_null() {
            None
        } else {
            Some((*desc).flags)
        }
    }
}

/// The rows of one plane of a frame, validated up front: each item is the
/// first `row_bytes` bytes of a row, top row first, whatever the sign of the
/// plane's linesize.
pub struct PlaneRows<'a> {
    first: *const u8,
    linesize: isize,
    rows: usize,
    row_bytes: usize,
    next: usize,
    _frame: PhantomData<&'a Video>,
}

impl<'a> Iterator for PlaneRows<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.next >= self.rows {
            return None;
        }
        let row = isize::try_from(self.next).ok()?;
        self.next += 1;
        // Validated in `plane_rows`: `row * linesize` cannot overflow.
        let delta = row.checked_mul(self.linesize)?;
        #[allow(unsafe_code)]
        // SAFETY: `plane_rows` proved that for every `row < rows` the span
        // `[first + row * linesize, + row_bytes)` lies inside a single
        // `AVBufferRef` owned by the frame, so the offset stays in that
        // allocation and the bytes are initialized, readable libav memory.
        // `PhantomData<&'a Video>` keeps the frame (and its buffers) borrowed,
        // immutably, for as long as the slice lives.
        unsafe {
            Some(core::slice::from_raw_parts(
                self.first.offset(delta),
                self.row_bytes,
            ))
        }
    }
}

/// Height in rows of `plane` of a `height`-row frame of `format`.
fn plane_height(format: ffi::AVPixelFormat, plane: usize, height: usize) -> Option<usize> {
    if plane != 1 && plane != 2 {
        return Some(height);
    }
    #[allow(unsafe_code)]
    // SAFETY: static descriptor table read, null-checked (see above).
    let shift = unsafe {
        let desc = ffi::av_pix_fmt_desc_get(format);
        if desc.is_null() {
            return None;
        }
        (*desc).log2_chroma_h
    };
    let round = (1_usize << shift) - 1;
    Some(height.checked_add(round)? >> shift)
}

/// The buffers a frame owns, `(data address, size)`: `buf[0..8]` then
/// `extended_buf[0..nb_extended_buf]`, read lazily one ref per step — no
/// heap allocation, so plane validation stays allocation-free on the
/// steady-state `run_into` path.
struct OwnedBuffers<'a> {
    raw: *const ffi::AVFrame,
    next: usize,
    _frame: PhantomData<&'a ffi::AVFrame>,
}

/// Number of `AVFrame::buf` slots (`AV_NUM_DATA_POINTERS`).
const BUF_SLOTS: usize = 8;

impl Iterator for OwnedBuffers<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<(usize, usize)> {
        loop {
            let index = self.next;
            #[allow(unsafe_code)]
            // SAFETY: `raw` is a live, non-null frame borrowed by the caller
            // for the iterator's lifetime; `buf` has `BUF_SLOTS` entries,
            // each null or a valid `AVBufferRef`; `extended_buf`, when
            // non-null, holds `nb_extended_buf` valid entries and `index` is
            // bounded by that count before the `add`. Only the address and
            // size are copied out.
            let slot = unsafe {
                if index < BUF_SLOTS {
                    Some(
                        (*self.raw)
                            .buf
                            .get(index)
                            .copied()
                            .unwrap_or(core::ptr::null_mut()),
                    )
                } else {
                    let extended = (*self.raw).extended_buf;
                    let n = usize::try_from((*self.raw).nb_extended_buf).unwrap_or(0);
                    let i = index - BUF_SLOTS;
                    if extended.is_null() || i >= n {
                        None
                    } else {
                        Some(*extended.add(i))
                    }
                }
            };
            self.next = index.checked_add(1)?;
            let b = slot?;
            #[allow(unsafe_code)]
            // SAFETY: `b` is null or a valid `AVBufferRef` (see above).
            let span = unsafe {
                if b.is_null() || (*b).data.is_null() {
                    None
                } else {
                    Some(((*b).data.expose_provenance(), (*b).size))
                }
            };
            if span.is_some() {
                return span;
            }
        }
    }
}

/// The buffers a frame owns (see [`OwnedBuffers`]); `raw` must be a live,
/// non-null frame that outlives the iterator.
fn owned_buffers<'a>(raw: *const ffi::AVFrame) -> OwnedBuffers<'a> {
    OwnedBuffers {
        raw,
        next: 0,
        _frame: PhantomData,
    }
}

/// Validate `plane` of `frame` and iterate its rows, `row_bytes` bytes each.
///
/// Checks, before any slice exists: the plane index is one of the format's
/// planes; the linesize is non-zero and `|linesize| >= row_bytes`; the row
/// count is the format's plane height (chroma subsampling applied); and the
/// first and last rows (hence every row between, the stride being constant)
/// lie inside one buffer the frame holds a reference to. A frame without
/// refcounted buffers is refused.
///
/// # Errors
/// [`FfmpegError::FrameLayout`] naming the first check that failed.
pub fn plane_rows(frame: &Video, plane: usize, row_bytes: usize) -> Result<PlaneRows<'_>> {
    padded_plane_rows(frame, plane, row_bytes, 0)
}

/// [`plane_rows`], additionally requiring `pad` bytes after the plane's
/// highest active byte (the end of the last row for a forward plane, of
/// row 0 for a bottom-up one) inside the same owning buffer as the rows.
/// The rows yielded never include the padding.
fn padded_plane_rows(
    frame: &Video,
    plane: usize,
    row_bytes: usize,
    pad: usize,
) -> Result<PlaneRows<'_>> {
    let no_rows = || PlaneRows {
        first: core::ptr::null(),
        linesize: 0,
        rows: 0,
        row_bytes: 0,
        next: 0,
        _frame: PhantomData,
    };
    if plane >= 8 {
        return Err(FfmpegError::FrameLayout("plane index out of range"));
    }
    #[allow(unsafe_code)]
    // SAFETY: the pointer is `frame`'s own `AVFrame`, borrowed for this call;
    // null-checked before the field reads; `plane < AV_NUM_DATA_POINTERS`.
    let (raw, height, data, linesize) = unsafe {
        let raw = frame.as_ptr();
        if raw.is_null() {
            return Err(FfmpegError::FrameLayout("frame is not allocated"));
        }
        (
            raw,
            (*raw).height,
            (*raw).data.get(plane).copied(),
            (*raw).linesize.get(plane).copied(),
        )
    };
    let (Some(data), Some(linesize)) = (data, linesize) else {
        return Err(FfmpegError::FrameLayout("plane index out of range"));
    };
    let av_format = ffi::AVPixelFormat::from(frame.format());
    #[allow(unsafe_code)]
    // SAFETY: static format table read.
    let planes = unsafe { ffi::av_pix_fmt_count_planes(av_format) };
    if planes <= 0 || plane >= usize::try_from(planes).unwrap_or(0) {
        return Err(FfmpegError::FrameLayout("plane is not part of the format"));
    }
    let height =
        usize::try_from(height).map_err(|_| FfmpegError::FrameLayout("negative height"))?;
    let rows = plane_height(av_format, plane, height)
        .ok_or(FfmpegError::FrameLayout("unknown pixel format"))?;
    if rows == 0 || row_bytes == 0 {
        return Ok(no_rows());
    }
    if data.is_null() {
        return Err(FfmpegError::FrameLayout("plane has no data"));
    }
    let stride = isize::try_from(linesize).map_err(|_| FfmpegError::FrameLayout("linesize"))?;
    if stride == 0 || stride.unsigned_abs() < row_bytes {
        return Err(FfmpegError::FrameLayout("linesize narrower than one row"));
    }
    // Address arithmetic in i128 so no step can overflow; nothing is
    // dereferenced or offset until the whole span is proven in-buffer.
    let base = i128::try_from(data.expose_provenance())
        .map_err(|_| FfmpegError::FrameLayout("address"))?;
    let last_row = i128::try_from(rows - 1).map_err(|_| FfmpegError::FrameLayout("rows"))?;
    let step = i128::try_from(stride).map_err(|_| FfmpegError::FrameLayout("linesize"))?;
    let width = i128::try_from(row_bytes).map_err(|_| FfmpegError::FrameLayout("row"))?;
    let pad = i128::try_from(pad).map_err(|_| FfmpegError::FrameLayout("padding"))?;
    let last = base + last_row * step;
    let (low, high) = (base.min(last), base.max(last) + width);
    let fits = |end: i128| {
        owned_buffers(raw).any(|(start, size)| {
            let (Ok(start), Ok(size)) = (i128::try_from(start), i128::try_from(size)) else {
                return false;
            };
            low >= start && end <= start + size
        })
    };
    if !fits(high) {
        return Err(FfmpegError::FrameLayout(
            "plane rows lie outside the frame's buffers",
        ));
    }
    if pad > 0 && !fits(high + pad) {
        return Err(FfmpegError::FrameLayout(
            "plane lacks the 16 bytes of padding libswscale may access past it",
        ));
    }
    // `rows - 1` times `stride` is inside one allocation, so it fits isize.
    if isize::try_from(last_row * step).is_err() {
        return Err(FfmpegError::FrameLayout("plane span overflows"));
    }
    Ok(PlaneRows {
        first: data.cast_const(),
        linesize: stride,
        rows,
        row_bytes,
        next: 0,
        _frame: PhantomData,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn alloc_video_checks_size_and_format_before_touching_planes() {
        let frame = alloc_video(Pixel::YUVA420P, 7, 5).unwrap();
        assert_eq!(frame.planes(), 4);
        assert!(matches!(
            alloc_video(Pixel::GRAY8, 0, 4),
            Err(FfmpegError::FrameLayout(_))
        ));
        assert!(matches!(
            alloc_video(Pixel::GRAY8, MAX_FRAME_DIMENSION + 1, 4),
            Err(FfmpegError::FrameLayout(_))
        ));
        assert!(matches!(
            alloc_video(Pixel::GRAY8, u32::MAX, u32::MAX),
            Err(FfmpegError::FrameLayout(_))
        ));
        assert!(matches!(
            alloc_video(Pixel::None, 4, 4),
            Err(FfmpegError::FrameLayout(_))
        ));
        assert!(matches!(
            alloc_video(Pixel::CUDA, 4, 4),
            Err(FfmpegError::FrameLayout(_))
        ));
    }

    #[test]
    fn plane_rows_walks_a_subsampled_plane_and_refuses_bad_requests() {
        let mut frame = alloc_video(Pixel::YUV420P, 6, 5).unwrap();
        let stride = frame.stride(1);
        for (i, b) in frame.data_mut(1).iter_mut().enumerate() {
            *b = u8::try_from(i / stride).unwrap();
        }
        // Chroma of a 5-row 4:2:0 frame has 3 rows of 3 samples.
        let rows: Vec<&[u8]> = plane_rows(&frame, 1, 3).unwrap().collect();
        assert_eq!(rows, [&[0_u8, 0, 0][..], &[1, 1, 1], &[2, 2, 2]]);
        assert!(plane_rows(&frame, 3, 1).is_err(), "no plane 3 in yuv420p");
        assert!(plane_rows(&frame, 9, 1).is_err());
        assert!(plane_rows(&frame, 0, stride.max(64) + 1).is_err());
        // An unallocated frame has no rows to give.
        let empty = Video::empty();
        assert!(plane_rows(&empty, 0, 1).is_err());
    }
}
