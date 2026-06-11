//! A borrowed, read-only view over the **luma (Y) plane** of an NV12 frame, plus
//! the normalized detection zone the picture probes analyse.
//!
//! Probes inspect only luma — black/freeze are luma-domain conditions and luma
//! is the first, tightly-packed plane of NV12 (invariant #5: frames stay NV12;
//! we never materialize RGBA). The view borrows the caller's buffer, so an
//! engine that has already *sampled* a tile's last-good frame can hand its Y
//! plane straight to a probe with **zero copy** and zero allocation.
//!
//! The view is purely descriptive arithmetic over a `&[u8]`; it performs no I/O
//! and cannot block, upholding the probe isolation contract.
use serde::{Deserialize, Serialize};

/// Why constructing a [`LumaView`] or [`DetectionZone`] failed.
///
/// Not `Eq` because [`LumaViewError::ZoneOutOfRange`] carries `f32` fields (which
/// are only `PartialEq`).
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum LumaViewError {
    /// `width` or `height` was zero — there is no picture to probe.
    #[error("luma view dimensions must be non-zero (got {width}x{height})")]
    ZeroDimension {
        /// Offending width.
        width: u32,
        /// Offending height.
        height: u32,
    },
    /// `stride` was smaller than `width`, so a row cannot hold `width` samples.
    #[error("luma stride {stride} is smaller than width {width}")]
    StrideTooSmall {
        /// The provided stride (bytes per row).
        stride: u32,
        /// The required minimum (the row width in samples).
        width: u32,
    },
    /// The backing slice is too small for `stride * height` bytes.
    #[error("luma buffer has {len} bytes but stride {stride} x height {height} needs {needed}")]
    BufferTooSmall {
        /// Length of the provided slice.
        len: usize,
        /// The row stride in bytes.
        stride: u32,
        /// The plane height in rows.
        height: u32,
        /// The minimum byte count required.
        needed: usize,
    },
    /// A detection zone fell outside the unit square or had non-positive extent.
    #[error("detection zone {x},{y} {w}x{h} is not within the unit square with positive extent")]
    ZoneOutOfRange {
        /// Left edge fraction.
        x: f32,
        /// Top edge fraction.
        y: f32,
        /// Width fraction.
        w: f32,
        /// Height fraction.
        h: f32,
    },
}

/// A normalized rectangular region of a frame to analyse, in `0.0..=1.0`
/// fractions of the picture (the broadcast "detection zone" / "active picture
/// window"). The full frame is `{ x: 0, y: 0, w: 1, h: 1 }` ([`Self::FULL`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DetectionZone {
    /// Left edge as a fraction of width.
    pub x: f32,
    /// Top edge as a fraction of height.
    pub y: f32,
    /// Width as a fraction of width.
    pub w: f32,
    /// Height as a fraction of height.
    pub h: f32,
}

impl Default for DetectionZone {
    fn default() -> Self {
        Self::FULL
    }
}

impl DetectionZone {
    /// The whole frame.
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    /// Construct and validate a detection zone.
    ///
    /// # Errors
    ///
    /// Returns [`LumaViewError::ZoneOutOfRange`] if any field is non-finite, the
    /// extent is non-positive (`w <= 0` or `h <= 0`), or the rectangle leaves the
    /// unit square (`x < 0`, `y < 0`, `x + w > 1`, `y + h > 1`).
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Result<Self, LumaViewError> {
        let finite = x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite();
        let in_range = x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0 && x + w <= 1.0 && y + h <= 1.0;
        if finite && in_range {
            Ok(Self { x, y, w, h })
        } else {
            Err(LumaViewError::ZoneOutOfRange { x, y, w, h })
        }
    }

    /// Resolve this fractional zone to integer pixel bounds within a
    /// `width`x`height` plane, clamped to the plane and guaranteed non-empty
    /// (at least one column and one row).
    ///
    /// Returns `(x0, y0, x1, y1)` with `x0 < x1 <= width` and `y0 < y1 <=
    /// height` (half-open ranges).
    #[must_use]
    pub fn pixel_bounds(self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        // f32 fraction * dimension, floored, clamped into range. Dimensions are
        // real frame sizes (<= a few thousand), so the products are tiny and the
        // f64 round-trip is exact; clamping keeps every value a valid index.
        let scale = |frac: f32, dim: u32| -> u32 {
            let d = f64::from(dim);
            let v = (f64::from(frac) * d).floor().clamp(0.0, d);
            // v is a non-negative, integer-valued f64 in [0, dim]; round-trip it
            // through u32 without an `as` cast by stepping down from the dimension.
            f64_floor_to_u32(v, dim)
        };
        let x0 = scale(self.x, width).min(width.saturating_sub(1));
        let y0 = scale(self.y, height).min(height.saturating_sub(1));
        let x1 = scale(self.x + self.w, width).max(x0 + 1).min(width);
        let y1 = scale(self.y + self.h, height).max(y0 + 1).min(height);
        (x0, y0, x1, y1)
    }
}

/// A borrowed, read-only view over an NV12 luma plane.
///
/// Holds the plane geometry and a reference to the row-major `u8` samples. It is
/// `Copy`-cheap (a slice reference plus three integers) and never owns or copies
/// the pixels.
#[derive(Debug, Clone, Copy)]
pub struct LumaView<'a> {
    samples: &'a [u8],
    width: u32,
    height: u32,
    stride: u32,
}

impl<'a> LumaView<'a> {
    /// Construct a luma view, validating geometry against the backing slice.
    ///
    /// `stride` is the number of bytes per row (may exceed `width` for aligned
    /// plane layouts); `samples` must contain at least `stride * height` bytes.
    ///
    /// # Errors
    ///
    /// - [`LumaViewError::ZeroDimension`] if `width` or `height` is zero;
    /// - [`LumaViewError::StrideTooSmall`] if `stride < width`;
    /// - [`LumaViewError::BufferTooSmall`] if `samples` is shorter than
    ///   `stride * height`.
    pub fn new(
        samples: &'a [u8],
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Self, LumaViewError> {
        if width == 0 || height == 0 {
            return Err(LumaViewError::ZeroDimension { width, height });
        }
        if stride < width {
            return Err(LumaViewError::StrideTooSmall { stride, width });
        }
        // `usize::try_from` keeps the conversion total; a value that does not fit
        // `usize` (or whose product overflows) cannot be satisfied by any slice,
        // so it reports BufferTooSmall with the saturated requirement.
        let too_small = || LumaViewError::BufferTooSmall {
            len: samples.len(),
            stride,
            height,
            needed: usize::MAX,
        };
        let stride_usize = usize::try_from(stride).map_err(|_| too_small())?;
        let height_usize = usize::try_from(height).map_err(|_| too_small())?;
        let needed = stride_usize
            .checked_mul(height_usize)
            .ok_or_else(too_small)?;
        if samples.len() < needed {
            return Err(LumaViewError::BufferTooSmall {
                len: samples.len(),
                stride,
                height,
                needed,
            });
        }
        Ok(Self {
            samples,
            width,
            height,
            stride,
        })
    }

    /// Construct a view over a tightly-packed plane (`stride == width`).
    ///
    /// # Errors
    ///
    /// As [`LumaView::new`] with `stride = width`.
    pub fn packed(samples: &'a [u8], width: u32, height: u32) -> Result<Self, LumaViewError> {
        Self::new(samples, width, height, width)
    }

    /// Frame width in samples.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in rows.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// The luma sample at `(x, y)`, or [`None`] if out of bounds.
    ///
    /// Bounds-checked and never panics (no `indexing_slicing`): an out-of-range
    /// coordinate yields [`None`].
    #[must_use]
    pub fn sample(&self, x: u32, y: u32) -> Option<u8> {
        if x >= self.width || y >= self.height {
            return None;
        }
        // `usize::try_from` (not `as`) keeps the conversion total on every target
        // width; a 16-bit target where a u32 index doesn't fit yields `None`.
        let y = usize::try_from(y).ok()?;
        let x = usize::try_from(x).ok()?;
        let stride = usize::try_from(self.stride).ok()?;
        let offset = y.checked_mul(stride)?.checked_add(x)?;
        self.samples.get(offset).copied()
    }

    /// The mean luma over a detection `zone`, as a `0.0..=255.0` value.
    ///
    /// Iterates the resolved pixel window and averages the 8-bit samples. The
    /// window is guaranteed non-empty by [`DetectionZone::pixel_bounds`], so the
    /// divisor is never zero.
    #[must_use]
    pub fn mean_luma(&self, zone: DetectionZone) -> f64 {
        let (x0, y0, x1, y1) = zone.pixel_bounds(self.width, self.height);
        // Accumulate directly in f64: `f64::from(u8)` and `+ 1.0` are exact for
        // every count a real frame can produce, so no `u64 -> f64` cast (banned
        // by `as_conversions`) is needed and no precision is lost.
        let mut sum = 0.0_f64;
        let mut count = 0.0_f64;
        for y in y0..y1 {
            for x in x0..x1 {
                if let Some(v) = self.sample(x, y) {
                    sum += f64::from(v);
                    count += 1.0;
                }
            }
        }
        if count == 0.0 {
            // Unreachable for a valid zone (bounds are non-empty), but stays
            // total rather than dividing by zero.
            return 0.0;
        }
        sum / count
    }

    /// The fraction of samples in `zone` that differ from `previous` by more than
    /// `tolerance` 8-bit levels, in `0.0..=1.0`.
    ///
    /// This is the inter-frame change metric the [`crate::probe::FreezeProbe`]
    /// uses: a value at or below the freeze threshold means the picture has not
    /// changed (frozen). Both views must describe the **same geometry**; if they
    /// differ, every comparable sample is treated as changed and the result is
    /// `1.0` (definitely not frozen — fail safe toward "live").
    #[must_use]
    pub fn changed_fraction(
        &self,
        previous: &LumaView<'_>,
        zone: DetectionZone,
        tolerance: u8,
    ) -> f64 {
        if self.width != previous.width || self.height != previous.height {
            return 1.0;
        }
        let (x0, y0, x1, y1) = zone.pixel_bounds(self.width, self.height);
        // Accumulate in f64 (each `+= 1.0` is exact within a frame's pixel
        // count) so no `u64 -> f64` cast is required.
        let mut changed = 0.0_f64;
        let mut count = 0.0_f64;
        for y in y0..y1 {
            for x in x0..x1 {
                if let (Some(cur), Some(prev)) = (self.sample(x, y), previous.sample(x, y)) {
                    if cur.abs_diff(prev) > tolerance {
                        changed += 1.0;
                    }
                } else {
                    // A missing sample on either side counts as changed: fail
                    // safe toward "not frozen".
                    changed += 1.0;
                }
                count += 1.0;
            }
        }
        if count == 0.0 {
            return 1.0;
        }
        changed / count
    }
}

/// Convert a non-negative, integer-valued `f64` (already clamped into
/// `0.0..=f64::from(max)`) to `u32` without an `as` cast.
///
/// Steps down from `max`: finds the largest `u32` value whose `f64`
/// representation does not exceed `value`. For the small magnitudes involved
/// (frame dimensions, never more than a few thousand) this is exact, and it is
/// total — a non-finite or out-of-range input clamps to `0..=max`.
fn f64_floor_to_u32(value: f64, max: u32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    if value >= f64::from(max) {
        return max;
    }
    // value is in (0, max); binary-search the integer floor without `as`.
    let mut lo: u32 = 0;
    let mut hi: u32 = max;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if f64::from(mid) <= value {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}
