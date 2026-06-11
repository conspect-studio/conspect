//! Confidence scopes: pure analysis over caller-supplied sample data producing
//! a drawable model.
//!
//! Multiview's scopes are computed in the pure model layer from byte samples the
//! caller extracts from a frame (luma for the [`Waveform`]/[`Histogram`],
//! chroma for the [`Vectorscope`], interleaved RGB for the [`RgbParade`]).
//! Keeping them pure means they are exactly unit-testable against known signals
//! and carry no GPU/native dependency — the renderer turns the returned counts
//! into bars/dots.
//!
//! * [`Histogram`] — a luma value distribution over `BINS` buckets.
//! * [`Waveform`] — per-column luma min/max/mean (a luma "waveform monitor").
//! * [`Vectorscope`] — a 2-D Cb/Cr distribution (`BINS` × `BINS`).
//! * [`RgbParade`] — three side-by-side per-channel histograms.
//!
//! `BINS` must divide 256 for the downscaling to be exact; the constructors
//! accept any `BINS >= 1` but the canonical sizes are 256, 64, 32.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Map an 8-bit sample value into one of `BINS` buckets.
///
/// `value * BINS / 256` (integer), clamped into `0..BINS`. For `BINS` that
/// divides 256 this is an exact, uniform bucketing.
fn bin_of<const BINS: usize>(value: u8) -> usize {
    // value is 0..=255, BINS is a small positive constant; the product fits a
    // usize on every supported platform (usize::from(u8) is lossless).
    let idx = usize::from(value) * BINS / 256;
    if idx >= BINS {
        BINS - 1
    } else {
        idx
    }
}

/// A luma-value histogram over `BINS` buckets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram<const BINS: usize> {
    /// Per-bucket sample counts, low value to high.
    pub bins: [u64; BINS],
}

impl<const BINS: usize> Histogram<BINS> {
    /// Build a histogram from luma (or any single-channel 8-bit) samples.
    #[must_use]
    pub fn from_luma(samples: &[u8]) -> Self {
        let mut bins = [0u64; BINS];
        for &v in samples {
            let idx = bin_of::<BINS>(v);
            // idx is provably < BINS (bin_of clamps), so this never overflows.
            if let Some(slot) = bins.get_mut(idx) {
                *slot = slot.saturating_add(1);
            }
        }
        Self { bins }
    }

    /// The total number of samples accumulated.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.bins.iter().sum()
    }

    /// The representative 8-bit value of the most-populated bucket, or [`None`]
    /// if no samples were accumulated. Ties resolve to the lowest value.
    #[must_use]
    pub fn peak_bin(&self) -> Option<u8> {
        let mut best_idx = None;
        let mut best_count = 0u64;
        for (i, &count) in self.bins.iter().enumerate() {
            if count > best_count {
                best_count = count;
                best_idx = Some(i);
            }
        }
        best_idx.map(bucket_centre::<BINS>)
    }
}

/// The representative 8-bit value at the centre of bucket `idx` of `BINS`.
fn bucket_centre<const BINS: usize>(idx: usize) -> u8 {
    // (idx * 256 + 128) / BINS, clamped to 0..=255. For BINS dividing 256 and a
    // full bucket this recovers the value exactly for the single-value case.
    let raw = (idx * 256 + 128) / BINS;
    u8::try_from(raw.min(255)).unwrap_or(u8::MAX)
}

/// Per-column statistics for a [`Waveform`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveformColumn {
    /// Minimum luma in the column.
    pub min: u8,
    /// Maximum luma in the column.
    pub max: u8,
    /// Mean luma in the column (rounded toward zero).
    pub mean: u8,
}

/// A luma waveform monitor: per-image-column min/max/mean luma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waveform {
    /// One entry per image column, left to right.
    pub columns: Vec<WaveformColumn>,
}

impl Waveform {
    /// Build a waveform from row-major luma of size `width` × `height`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidScope`] if `luma.len() != width * height` or
    /// either dimension is zero.
    pub fn from_luma(luma: &[u8], width: usize, height: usize) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidScope(
                "waveform dimensions must be non-zero".to_owned(),
            ));
        }
        let expected = width
            .checked_mul(height)
            .ok_or_else(|| Error::InvalidScope("waveform dimensions overflow".to_owned()))?;
        if luma.len() != expected {
            return Err(Error::InvalidScope(format!(
                "luma length {} != width*height {expected}",
                luma.len()
            )));
        }
        let mut columns = Vec::with_capacity(width);
        for col in 0..width {
            let mut min = u8::MAX;
            let mut max = u8::MIN;
            let mut sum = 0u64;
            for row in 0..height {
                let v = *luma.get(row * width + col).unwrap_or(&0);
                min = min.min(v);
                max = max.max(v);
                sum += u64::from(v);
            }
            // height > 0, so the division is well-defined and the quotient <= 255.
            let mean_u64 = sum / u64::try_from(height).unwrap_or(1);
            let mean = u8::try_from(mean_u64).unwrap_or(u8::MAX);
            columns.push(WaveformColumn { min, max, mean });
        }
        Ok(Self { columns })
    }
}

/// A 2-D Cb/Cr distribution (`BINS` × `BINS`): a vectorscope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vectorscope<const BINS: usize> {
    /// `bins[cr_bucket][cb_bucket]` sample counts. Indexed Cr (vertical) by Cb
    /// (horizontal) so a renderer maps it straight onto the polar display.
    pub bins: Vec<[u64; BINS]>,
}

impl<const BINS: usize> Vectorscope<BINS> {
    /// Build a vectorscope from parallel Cb / Cr sample planes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidScope`] if `cb.len() != cr.len()`.
    pub fn from_chroma(cb: &[u8], cr: &[u8]) -> Result<Self> {
        if cb.len() != cr.len() {
            return Err(Error::InvalidScope(format!(
                "cb/cr length mismatch ({} vs {})",
                cb.len(),
                cr.len()
            )));
        }
        let mut bins = vec![[0u64; BINS]; BINS];
        for (&blue_diff, &red_diff) in cb.iter().zip(cr.iter()) {
            let vertical = bin_of::<BINS>(red_diff);
            let horizontal = bin_of::<BINS>(blue_diff);
            if let Some(row) = bins.get_mut(vertical) {
                if let Some(slot) = row.get_mut(horizontal) {
                    *slot = slot.saturating_add(1);
                }
            }
        }
        Ok(Self { bins })
    }

    /// The `(cr_bucket, cb_bucket)` index a `(cb, cr)` pair falls into.
    #[must_use]
    pub fn bin_index(&self, cb: u8, cr: u8) -> (usize, usize) {
        (bin_of::<BINS>(cr), bin_of::<BINS>(cb))
    }
}

/// Which channel of an [`RgbParade`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ParadeChannel {
    /// Red channel.
    Red,
    /// Green channel.
    Green,
    /// Blue channel.
    Blue,
}

impl ParadeChannel {
    /// A single-letter text label (accessibility: identify by letter, not just
    /// colour).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Red => "R",
            Self::Green => "G",
            Self::Blue => "B",
        }
    }
}

/// The parade presentation kind (currently RGB; component parades can be added).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Parade {
    /// An RGB parade (R, G, B histograms side by side).
    #[default]
    Rgb,
}

/// An RGB parade: one [`Histogram`] per channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbParade<const BINS: usize> {
    /// Red-channel histogram.
    pub red: Histogram<BINS>,
    /// Green-channel histogram.
    pub green: Histogram<BINS>,
    /// Blue-channel histogram.
    pub blue: Histogram<BINS>,
}

impl<const BINS: usize> RgbParade<BINS> {
    /// Build an RGB parade from interleaved `[R, G, B, R, G, B, ...]` samples.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidScope`] if `rgb.len()` is not a multiple of 3.
    pub fn from_rgb(rgb: &[u8]) -> Result<Self> {
        if rgb.len() % 3 != 0 {
            return Err(Error::InvalidScope(format!(
                "rgb length {} is not a multiple of 3",
                rgb.len()
            )));
        }
        let mut r = [0u64; BINS];
        let mut g = [0u64; BINS];
        let mut b = [0u64; BINS];
        for px in rgb.chunks_exact(3) {
            // chunks_exact(3) guarantees three elements per chunk.
            accumulate::<BINS>(&mut r, *px.first().unwrap_or(&0));
            accumulate::<BINS>(&mut g, *px.get(1).unwrap_or(&0));
            accumulate::<BINS>(&mut b, *px.get(2).unwrap_or(&0));
        }
        Ok(Self {
            red: Histogram { bins: r },
            green: Histogram { bins: g },
            blue: Histogram { bins: b },
        })
    }
}

/// Increment the bucket `value` falls into within `bins`.
fn accumulate<const BINS: usize>(bins: &mut [u64; BINS], value: u8) {
    let idx = bin_of::<BINS>(value);
    if let Some(slot) = bins.get_mut(idx) {
        *slot = slot.saturating_add(1);
    }
}
