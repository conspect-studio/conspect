//! Pure mapping helpers between `libav*` types and [`multiview_core`] types.
//!
//! These functions own **no** libav object and perform **no** FFI; they only
//! translate value types (rationals, pixel formats, the four color axes). That
//! keeps the conversion logic unit-testable without touching the native layer
//! and isolates the "what does CICP code-point N mean" knowledge in one place.
//!
//! ## Timing discipline (invariant #3)
//! A libav timebase is carried as [`multiview_core::time::Rational`] (exact
//! `num/den`), never as a float fps. [`to_ff_rational`] converts the other way
//! for configuring encoders/streams, narrowing the `i64` numerator/denominator
//! into the `i32` that `AVRational` uses and reporting [`FfmpegError::Rational`]
//! if a (degenerate) value does not fit — it is never silently truncated.

use ffmpeg::format::Pixel;
use ffmpeg::{color as ff_color, media, Rational as FfRational};
use ffmpeg_next as ffmpeg;

use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use multiview_core::pixel::PixelFormat;
use multiview_core::stream::{CoarseMediaKind, StreamKind};
use multiview_core::time::Rational;

use crate::error::FfmpegError;

/// The kind of media carried by a stream, mirrored from libav's `AVMediaType`
/// but reduced to the variants Multiview acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaKind {
    /// A video stream.
    Video,
    /// An audio stream.
    Audio,
    /// A subtitle stream (teletext, DVB-sub, CEA-608/708 carried as its own
    /// stream, `WebVTT`, …) — the caption decoders consume these.
    Subtitle,
    /// Any other media type (data, attachment, unknown).
    Other,
}

impl From<media::Type> for MediaKind {
    fn from(value: media::Type) -> Self {
        match value {
            media::Type::Video => Self::Video,
            media::Type::Audio => Self::Audio,
            media::Type::Subtitle => Self::Subtitle,
            _ => Self::Other,
        }
    }
}

/// Bridge the libav [`MediaKind`] to the core-level [`CoarseMediaKind`] so the
/// canonical [`StreamKind`] classification (which lives in `multiview-core`,
/// dependency-free) can be reused without core depending on this crate.
impl From<MediaKind> for CoarseMediaKind {
    fn from(value: MediaKind) -> Self {
        // `MediaKind` is `#[non_exhaustive]` but defined in this crate, so this
        // match is exhaustive here; adding a future variant deliberately breaks
        // compilation so a new libav media kind is classified, never silently
        // dropped into `Other`.
        match value {
            MediaKind::Video => Self::Video,
            MediaKind::Audio => Self::Audio,
            MediaKind::Subtitle => Self::Subtitle,
            MediaKind::Other => Self::Other,
        }
    }
}

/// Lift a libav [`MediaKind`] to the canonical [`StreamKind`], **without** a
/// codec name.
///
/// `Video`/`Audio`/`Subtitle` lift 1:1; an `Other` stream becomes a generic
/// [`StreamKind::Data`] passthrough (so it stays addressable rather than being
/// dropped the way `best_stream` drops `Other`). Callers that have the codec
/// name should prefer [`stream_kind_from_media_and_codec`] to refine `Other`
/// into the SCTE-35 / KLV / timecode routing kinds.
impl From<MediaKind> for StreamKind {
    fn from(value: MediaKind) -> Self {
        Self::from(CoarseMediaKind::from(value))
    }
}

/// Classify an elementary stream into the canonical [`StreamKind`] from its
/// already-available libav [`MediaKind`] **and** codec name — pure metadata
/// classification, **no decode**.
///
/// This is the `from_media_and_codec` lift named in ADR-0034 §1. The codec name
/// is the libav descriptor name (e.g. from `demux::codec_id_name`); it refines a
/// `kind = Other` stream into [`StreamKind::Data`] (`scte_35` → `Scte35`,
/// `klv`/`smpte_klv` → `Klv`) or [`StreamKind::Timecode`] (`timed_id3` /
/// `timecode`). The actual mapping table lives in
/// [`StreamKind::from_coarse_and_codec`] (`multiview-core`) so it is testable
/// without the native layer; this function only bridges the libav kind.
#[must_use]
pub fn stream_kind_from_media_and_codec(kind: MediaKind, codec_name: &str) -> StreamKind {
    StreamKind::from_coarse_and_codec(CoarseMediaKind::from(kind), codec_name)
}

/// Convert a libav [`Rational`](FfRational) (an `i32/i32` timebase or rate) into
/// a [`multiview_core`] [`Rational`].
///
/// Widening `i32 -> i64` is always lossless, so this is total.
#[must_use]
pub fn from_ff_rational(value: FfRational) -> Rational {
    Rational::new(i64::from(value.numerator()), i64::from(value.denominator()))
}

/// Convert a [`multiview_core`] [`Rational`] into a libav [`Rational`](FfRational)
/// for configuring an encoder time-base or stream rate.
///
/// The value is reduced first, then each component is narrowed to the `i32`
/// that `AVRational` stores.
///
/// # Errors
/// Returns [`FfmpegError::Rational`] if the reduced numerator or denominator
/// does not fit in `i32` (only possible for pathological values — real
/// timebases such as `60000/1001` fit comfortably).
pub fn to_ff_rational(value: Rational) -> Result<FfRational, FfmpegError> {
    let reduced = value.reduce();
    let num = i32::try_from(reduced.num).map_err(|_| FfmpegError::Rational {
        num: reduced.num,
        den: reduced.den,
    })?;
    let den = i32::try_from(reduced.den).map_err(|_| FfmpegError::Rational {
        num: reduced.num,
        den: reduced.den,
    })?;
    Ok(FfRational::new(num, den))
}

/// Map a libav pixel format to the [`multiview_core`] [`PixelFormat`] where one of
/// the canonical working formats applies.
///
/// Only the formats Multiview carries on its own timeline are mapped; everything
/// else (including planar `YUV420P`, which the scaler converts to NV12 before it
/// enters the pipeline) returns [`None`] so callers can decide whether a
/// conversion is required rather than silently mislabeling the frame.
#[must_use]
pub fn pixel_to_multiview(format: Pixel) -> Option<PixelFormat> {
    match format {
        Pixel::NV12 => Some(PixelFormat::Nv12),
        Pixel::P010LE => Some(PixelFormat::P010),
        Pixel::RGBA => Some(PixelFormat::Rgba),
        _ => None,
    }
}

/// Map a [`multiview_core`] [`PixelFormat`] to its libav pixel format.
#[must_use]
pub fn pixel_to_ff(format: PixelFormat) -> Pixel {
    match format {
        PixelFormat::P010 => Pixel::P010LE,
        PixelFormat::Rgba => Pixel::RGBA,
        // `Nv12` plus the `#[non_exhaustive]` fallthrough: any future variant
        // defaults to the canonical NV12 working format until mapped explicitly.
        PixelFormat::Nv12 | _ => Pixel::NV12,
    }
}

/// Translate a libav color-space (matrix coefficients) code point into the
/// [`multiview_core`] [`MatrixCoefficients`] axis.
#[must_use]
fn matrix_from_ff(space: ff_color::Space) -> MatrixCoefficients {
    match space {
        ff_color::Space::BT709 => MatrixCoefficients::Bt709,
        ff_color::Space::BT470BG | ff_color::Space::SMPTE170M => MatrixCoefficients::Bt601,
        ff_color::Space::BT2020NCL => MatrixCoefficients::Bt2020Ncl,
        ff_color::Space::RGB => MatrixCoefficients::Rgb,
        _ => MatrixCoefficients::Unspecified,
    }
}

/// Translate a libav primaries code point into the [`ColorPrimaries`] axis.
#[must_use]
fn primaries_from_ff(primaries: ff_color::Primaries) -> ColorPrimaries {
    match primaries {
        ff_color::Primaries::BT709 => ColorPrimaries::Bt709,
        ff_color::Primaries::SMPTE170M => ColorPrimaries::Bt601_525,
        ff_color::Primaries::BT470BG => ColorPrimaries::Bt601_625,
        ff_color::Primaries::BT2020 => ColorPrimaries::Bt2020,
        _ => ColorPrimaries::Unspecified,
    }
}

/// Translate a libav transfer-characteristic code point into the
/// [`TransferCharacteristic`] axis.
#[must_use]
fn transfer_from_ff(trc: ff_color::TransferCharacteristic) -> TransferCharacteristic {
    match trc {
        ff_color::TransferCharacteristic::BT709 => TransferCharacteristic::Bt709,
        ff_color::TransferCharacteristic::SMPTE170M => TransferCharacteristic::Bt601,
        ff_color::TransferCharacteristic::BT2020_10
        | ff_color::TransferCharacteristic::BT2020_12 => TransferCharacteristic::Bt2020,
        ff_color::TransferCharacteristic::IEC61966_2_1 => TransferCharacteristic::Srgb,
        ff_color::TransferCharacteristic::SMPTE2084 => TransferCharacteristic::Pq,
        ff_color::TransferCharacteristic::ARIB_STD_B67 => TransferCharacteristic::Hlg,
        _ => TransferCharacteristic::Unspecified,
    }
}

/// Translate a libav color-range code point into the [`ColorRange`] axis.
#[must_use]
fn range_from_ff(range: ff_color::Range) -> ColorRange {
    match range {
        ff_color::Range::MPEG => ColorRange::Limited,
        ff_color::Range::JPEG => ColorRange::Full,
        ff_color::Range::Unspecified => ColorRange::Unspecified,
    }
}

/// Build a [`ColorInfo`] (the four independent axes) from a decoded frame's
/// libav color tags.
///
/// This is the "detect 4 axes" step (invariant #8): each axis is read
/// independently and an untagged axis stays [`Unspecified`](ColorRange::Unspecified)
/// rather than being guessed here — the player-style defaulting lives in
/// [`ColorInfo::resolve_defaults`], applied later by the compositor against the
/// real geometry.
#[must_use]
pub fn color_from_ff(
    space: ff_color::Space,
    primaries: ff_color::Primaries,
    trc: ff_color::TransferCharacteristic,
    range: ff_color::Range,
) -> ColorInfo {
    ColorInfo {
        primaries: primaries_from_ff(primaries),
        transfer: transfer_from_ff(trc),
        matrix: matrix_from_ff(space),
        range: range_from_ff(range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntsc_rational_round_trips_through_libav_exactly() {
        // 59.94 fps = 60000/1001 must survive multiview -> libav -> multiview with no
        // drift (invariant #3: exact rationals, never float fps).
        let multiview = Rational::new(60_000, 1001);
        let ff = to_ff_rational(multiview).expect("60000/1001 fits an AVRational");
        assert_eq!(ff.numerator(), 60_000);
        assert_eq!(ff.denominator(), 1001);
        assert_eq!(from_ff_rational(ff), multiview);
    }

    #[test]
    fn to_ff_rational_reduces_before_narrowing() {
        // 120000/2002 reduces to 60000/1001, which fits even though the raw
        // numerator would also fit; the reduced form is what libav receives.
        let ff = to_ff_rational(Rational::new(120_000, 2002)).expect("reduces and fits");
        assert_eq!((ff.numerator(), ff.denominator()), (60_000, 1001));
    }

    #[test]
    fn to_ff_rational_rejects_values_that_overflow_i32() {
        // A numerator beyond i32::MAX that does not reduce cannot be an
        // AVRational; it must be a typed error, never a silent truncation.
        let huge = Rational::new(i64::from(i32::MAX) + 1, 1);
        match to_ff_rational(huge) {
            Err(FfmpegError::Rational { num, den }) => {
                assert_eq!(num, i64::from(i32::MAX) + 1);
                assert_eq!(den, 1);
            }
            other => panic!("expected FfmpegError::Rational, got {other:?}"),
        }
    }

    #[test]
    fn pixel_mapping_is_consistent_both_ways_for_nv12() {
        assert_eq!(pixel_to_multiview(Pixel::NV12), Some(PixelFormat::Nv12));
        assert_eq!(pixel_to_ff(PixelFormat::Nv12), Pixel::NV12);
        // YUV420P is intentionally NOT a canonical working format — it must map
        // to None so callers know a conversion is required.
        assert_eq!(pixel_to_multiview(Pixel::YUV420P), None);
    }

    #[test]
    fn color_axes_map_independently_and_preserve_unspecified() {
        // A BT.709 limited-range frame maps all four axes; an untagged axis must
        // stay Unspecified (the detect step never guesses — invariant #8).
        let info = color_from_ff(
            ff_color::Space::BT709,
            ff_color::Primaries::Unspecified,
            ff_color::TransferCharacteristic::BT709,
            ff_color::Range::MPEG,
        );
        assert_eq!(info.matrix, MatrixCoefficients::Bt709);
        assert_eq!(info.primaries, ColorPrimaries::Unspecified);
        assert_eq!(info.transfer, TransferCharacteristic::Bt709);
        assert_eq!(info.range, ColorRange::Limited);
    }

    #[test]
    fn smpte170m_space_maps_to_bt601_matrix() {
        let info = color_from_ff(
            ff_color::Space::SMPTE170M,
            ff_color::Primaries::SMPTE170M,
            ff_color::TransferCharacteristic::SMPTE170M,
            ff_color::Range::JPEG,
        );
        assert_eq!(info.matrix, MatrixCoefficients::Bt601);
        assert_eq!(info.primaries, ColorPrimaries::Bt601_525);
        assert_eq!(info.transfer, TransferCharacteristic::Bt601);
        assert_eq!(info.range, ColorRange::Full);
    }
}
