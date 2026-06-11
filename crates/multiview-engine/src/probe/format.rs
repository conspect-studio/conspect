//! The **format/standard-change** probe: an observed frame's geometry, pixel
//! format and colorimetry compared against what the operator expects (ADR-MV001
//! / broadcast-multiviewer §4 — "format/standard change … colorimetry/HDR
//! mismatch").
//!
//! Unlike the picture probes this needs no pixels: it compares the sampled
//! [`FrameMeta`](multiview_core::frame::FrameMeta) against an [`ExpectedFormat`].
//! Each color axis is checked independently, and an axis the operator left
//! **unspecified** (or one the *source* did not signal) is treated as "don't
//! care" so an under-specified expectation never produces a spurious mismatch.
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use multiview_core::frame::FrameMeta;
use multiview_core::pixel::PixelFormat;
use serde::{Deserialize, Serialize};

use super::ProbeObservation;
use multiview_core::alarm::AlarmKind;

/// The format a source is expected to deliver.
///
/// Every field is optional: `None` means "don't care", so an operator can pin
/// only the axes they care about (e.g. just resolution + frame size) and leave
/// the rest unconstrained. For the color axes, an expected value of
/// [`ColorPrimaries::Unspecified`] (etc.) is *also* treated as don't-care, and a
/// mismatch is only reported when **both** sides signal a value and they differ.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ExpectedFormat {
    /// Expected width in pixels (`None` = don't care).
    pub width: Option<u32>,
    /// Expected height in pixels (`None` = don't care).
    pub height: Option<u32>,
    /// Expected pixel format (`None` = don't care).
    pub pixel_format: Option<PixelFormat>,
    /// Expected color primaries (`None` or `Unspecified` = don't care).
    pub primaries: Option<ColorPrimaries>,
    /// Expected transfer characteristics (`None` or `Unspecified` = don't care).
    pub transfer: Option<TransferCharacteristic>,
    /// Expected matrix coefficients (`None` or `Unspecified` = don't care).
    pub matrix: Option<MatrixCoefficients>,
    /// Expected quantization range (`None` or `Unspecified` = don't care).
    pub range: Option<ColorRange>,
}

impl ExpectedFormat {
    /// Pin an expected frame size (width x height); leaves all other axes
    /// unconstrained.
    #[must_use]
    pub const fn with_size(width: u32, height: u32) -> Self {
        Self {
            width: Some(width),
            height: Some(height),
            pixel_format: None,
            primaries: None,
            transfer: None,
            matrix: None,
            range: None,
        }
    }

    /// Pin the expected color description (all four axes) on top of `self`.
    #[must_use]
    pub const fn with_color(mut self, color: ColorInfo) -> Self {
        self.primaries = Some(color.primaries);
        self.transfer = Some(color.transfer);
        self.matrix = Some(color.matrix);
        self.range = Some(color.range);
        self
    }
}

/// One axis along which an observed frame can differ from the expectation.
///
/// Serialised **tagged** by variant name (repo convention — never `untagged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FormatAxis {
    /// Frame width differed.
    Width,
    /// Frame height differed.
    Height,
    /// Pixel format differed.
    PixelFormat,
    /// Color primaries differed (both sides signalled and disagree).
    Primaries,
    /// Transfer characteristics differed.
    Transfer,
    /// Matrix coefficients differed.
    Matrix,
    /// Quantization range differed.
    Range,
}

/// Which axes of an observed frame did not match the expectation.
///
/// A [`FormatMismatch::is_clean`] value means every constrained axis matched.
/// The set lists exactly the axes that changed so the alarm/UI layer can describe
/// precisely what happened; the order is the canonical axis order (geometry,
/// pixel format, then the four color axes).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FormatMismatch {
    /// The axes that differed, in canonical order.
    pub axes: Vec<FormatAxis>,
}

impl FormatMismatch {
    /// Whether **no** axis mismatched (every constrained axis matched).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.axes.is_empty()
    }

    /// Whether **any** axis mismatched.
    #[must_use]
    pub fn any(&self) -> bool {
        !self.is_clean()
    }

    /// Whether a specific `axis` mismatched.
    #[must_use]
    pub fn contains(&self, axis: FormatAxis) -> bool {
        self.axes.contains(&axis)
    }

    /// The number of mismatching axes.
    #[must_use]
    pub fn count(&self) -> usize {
        self.axes.len()
    }
}

/// A stateless format/standard-change detector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FormatProbe {
    expected: ExpectedFormat,
}

impl FormatProbe {
    /// Create a probe checking against `expected`.
    #[must_use]
    pub const fn new(expected: ExpectedFormat) -> Self {
        Self { expected }
    }

    /// The expectation this probe enforces.
    #[must_use]
    pub const fn expected(&self) -> &ExpectedFormat {
        &self.expected
    }

    /// Compare an observed frame's metadata against the expectation, returning the
    /// set of mismatching axes ([`FormatMismatch`]) in canonical order.
    #[must_use]
    pub fn compare(&self, observed: &FrameMeta) -> FormatMismatch {
        let mut axes = Vec::new();
        if self.expected.width.is_some_and(|w| w != observed.width) {
            axes.push(FormatAxis::Width);
        }
        if self.expected.height.is_some_and(|h| h != observed.height) {
            axes.push(FormatAxis::Height);
        }
        if self
            .expected
            .pixel_format
            .is_some_and(|p| p != observed.format)
        {
            axes.push(FormatAxis::PixelFormat);
        }
        if mismatch_axis(
            self.expected.primaries,
            observed.color.primaries,
            ColorPrimaries::Unspecified,
        ) {
            axes.push(FormatAxis::Primaries);
        }
        if mismatch_axis(
            self.expected.transfer,
            observed.color.transfer,
            TransferCharacteristic::Unspecified,
        ) {
            axes.push(FormatAxis::Transfer);
        }
        if mismatch_axis(
            self.expected.matrix,
            observed.color.matrix,
            MatrixCoefficients::Unspecified,
        ) {
            axes.push(FormatAxis::Matrix);
        }
        if mismatch_axis(
            self.expected.range,
            observed.color.range,
            ColorRange::Unspecified,
        ) {
            axes.push(FormatAxis::Range);
        }
        FormatMismatch { axes }
    }

    /// Evaluate a sampled frame and produce a [`ProbeObservation`].
    ///
    /// `condition_present` is `true` when **any** constrained axis mismatched;
    /// `measured` is the number of mismatching axes, as an `f64` for diagnostics.
    #[must_use]
    pub fn detect(&self, observed: &FrameMeta) -> ProbeObservation {
        let mismatch = self.compare(observed);
        // The mismatch count is at most 7 (the axis count), so `u32::try_from`
        // never fails; `f64::from(u32)` is exact.
        let count = u32::try_from(mismatch.count()).unwrap_or(u32::MAX);
        ProbeObservation::new(AlarmKind::FormatMismatch, mismatch.any(), f64::from(count))
    }
}

/// A color axis mismatches when the operator pinned a **specific** value
/// (not `None`, not the axis `unspecified` sentinel), the source **signalled** a
/// specific value (not `unspecified`), and the two differ. Either side being
/// unspecified is "don't care" and never a mismatch.
fn mismatch_axis<T: PartialEq + Copy>(expected: Option<T>, observed: T, unspecified: T) -> bool {
    match expected {
        Some(want) if want != unspecified && observed != unspecified => want != observed,
        _ => false,
    }
}
