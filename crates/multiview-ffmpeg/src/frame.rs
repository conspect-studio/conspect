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
    let mut frame = Video::empty();
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

/// The buffers a frame owns: `buf[0..8]` plus `extended_buf`.
fn owned_buffers(raw: *const ffi::AVFrame) -> impl Iterator<Item = (usize, usize)> {
    #[allow(unsafe_code)]
    // SAFETY: `raw` is a live frame borrowed by the caller; `buf` entries are
    // null or valid `AVBufferRef`s, `extended_buf` holds `nb_extended_buf`
    // valid entries when non-null. Only addresses and sizes are copied out.
    let spans: Vec<(usize, usize)> = unsafe {
        let mut spans = Vec::new();
        let mut push = |b: *mut ffi::AVBufferRef| {
            if !b.is_null() && !(*b).data.is_null() {
                spans.push(((*b).data.expose_provenance(), (*b).size));
            }
        };
        for &b in &(*raw).buf {
            push(b);
        }
        let extended = (*raw).extended_buf;
        if !extended.is_null() {
            let n = usize::try_from((*raw).nb_extended_buf).unwrap_or(0);
            for i in 0..n {
                push(*extended.add(i));
            }
        }
        spans
    };
    spans.into_iter()
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
    let last = base + last_row * step;
    let (low, high) = (base.min(last), base.max(last) + width);
    let inside = owned_buffers(raw).any(|(start, size)| {
        let (Ok(start), Ok(size)) = (i128::try_from(start), i128::try_from(size)) else {
            return false;
        };
        low >= start && high <= start + size
    });
    if !inside {
        return Err(FfmpegError::FrameLayout(
            "plane rows lie outside the frame's buffers",
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
