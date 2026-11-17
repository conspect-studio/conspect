//! Pure probe validation for a runtime clip load (design 4.2, 4.3, section 6):
//! the facts a probe learned about a clip, and the exact-value checks the player
//! applies before it accepts the clip.
//!
//! This module is libav-free and thread-free, so it lives in the CI-green
//! default build and is unit-tested with no fixtures, no file IO and no
//! hardware. The `ffmpeg`-gated opener ([`super::opener`]) fills a
//! [`ProbedClip`] from the container and calls these functions; the decisions
//! themselves never touch libav.
//!
//! Two rules from the design are enforced here:
//!
//! * **Exact rational cadence (4.3, decision 2).** A clip is accepted only when
//!   its average frame rate, as an exact rational, equals the canvas cadence.
//!   `30000/1001` and `2997/100` are different rates; there is no float and no
//!   tolerance. A variable frame rate (`avg != r_frame_rate`) is rejected too.
//! * **No guessed length (4.2).** The out-point is the minimum of the declared
//!   and probed lengths; when neither is known the load is refused rather than
//!   guessed.

// The pure validation is consumed by the `ffmpeg`-gated opener. In the default
// build it is exercised only by this module's own unit tests (design 9.1 keeps
// those in the CI-green default build), so the non-test lib compile would flag
// every entry point dead — the crate's `-D warnings` CI gate would fail. Scoped
// to this module and to the non-ffmpeg build only.
#![cfg_attr(not(feature = "ffmpeg"), allow(dead_code))]

use multiview_core::time::Rational;
use thiserror::Error;

use super::{ClipSpec, PlayoutGeometry, PlayoutGeometryError};

/// The facts a probe established about one clip.
///
/// Cadence is always an exact rational, never a float fps (invariant #3). Every
/// field is diagnostic or feeds [`resolve_geometry`]; none borrows libav state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbedClip {
    /// The video codec name as reported by libav (diagnostic only).
    pub codec: String,
    /// Coded video width in pixels.
    pub width: u32,
    /// Coded video height in pixels.
    pub height: u32,
    /// The stream's average frame rate, exactly as declared.
    pub cadence: Rational,
    /// The probed frame count, when the container declares one.
    pub frames: Option<u64>,
    /// Whether the container carries at least one audio stream.
    pub has_audio: bool,
    /// Whether the video pixel format carries an alpha plane.
    pub has_alpha: bool,
    /// The pixel format name as reported by libav (diagnostic only).
    pub pix_fmt: String,
}

/// Why a probed clip was refused (design 4.2, 4.3, 8).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProbeError {
    /// The clip's cadence is not the canvas cadence. Both rates are named.
    #[error(
        "clip frame rate {got_num}/{got_den} does not match the canvas frame rate \
         {want_num}/{want_den}"
    )]
    Cadence {
        /// Canvas cadence numerator.
        want_num: i64,
        /// Canvas cadence denominator.
        want_den: i64,
        /// Clip cadence numerator.
        got_num: i64,
        /// Clip cadence denominator.
        got_den: i64,
    },
    /// The container declares no usable frame rate.
    #[error("clip declares no frame rate")]
    NoCadence,
    /// The clip is variable frame rate: its average and nominal rates differ.
    #[error(
        "clip is variable frame rate: average {avg_num}/{avg_den}, nominal {nom_num}/{nom_den}"
    )]
    Vfr {
        /// Average frame rate numerator.
        avg_num: i64,
        /// Average frame rate denominator.
        avg_den: i64,
        /// Nominal (`r_frame_rate`) numerator.
        nom_num: i64,
        /// Nominal frame rate denominator.
        nom_den: i64,
    },
    /// The clip carries alpha in a pixel format the player's NV12+A decode
    /// cannot carry (ADR-0058 §3, ADR-0112 §5): playing it would silently
    /// drop the alpha, so the load is refused with a remediation.
    #[error(
        "clip pixel format {pix_fmt} carries alpha the player cannot decode; \
         transcode to ProRes 4444, QuickTime Animation (qtrle), VP9 alpha (WebM) \
         or a PNG sequence"
    )]
    Alpha {
        /// The pixel format that carries alpha.
        pix_fmt: String,
    },
    /// The clip is HEVC with more than one layer: HEVC with alpha carries its
    /// alpha in an auxiliary layer that libav decodes opaque (ADR-0058 §3,
    /// Alternative E), so a stinger would cover Program. Refused rather than
    /// played opaque.
    #[error(
        "clip is HEVC with {layers} layers (HEVC alpha decodes opaque); \
         transcode to ProRes 4444, QuickTime Animation (qtrle), VP9 alpha (WebM) \
         or a PNG sequence"
    )]
    HevcAlpha {
        /// `vps_max_layers_minus1 + 1` from the stream's VPS.
        layers: u8,
    },
    /// The container has no video stream.
    #[error("container has no video stream")]
    NoVideo,
    /// The clip length is unknown and no out-point was declared.
    #[error("clip length is unknown and no declared out-point was given")]
    UnknownLength,
    /// The resolved playout window is not nested or the cadence is invalid.
    #[error(transparent)]
    Window(#[from] PlayoutGeometryError),
}

/// Whether two frame-rate rationals are exactly equal in value.
///
/// Equality is by cross-multiplication ([`Rational`] compares by value), so an
/// unreduced declaration still matches. An invalid rational (zero denominator)
/// never matches.
#[must_use]
pub(super) fn cadence_matches(canvas: Rational, stream: Rational) -> bool {
    canvas.is_valid() && stream.is_valid() && canvas == stream
}

/// Validate a clip's declared frame rate against the canvas, exactly.
///
/// `avg` is the stream's average frame rate and `nominal` its `r_frame_rate`
/// (both `None` when the container declares none). A variable frame rate is
/// rejected even when its average happens to match the canvas.
///
/// # Errors
///
/// * [`ProbeError::NoCadence`] — the container declares no average frame rate.
/// * [`ProbeError::Cadence`] — the average is not exactly the canvas cadence.
/// * [`ProbeError::Vfr`] — the average and nominal rates differ.
pub(super) fn validate_cadence(
    canvas: Rational,
    avg: Option<Rational>,
    nominal: Option<Rational>,
) -> Result<Rational, ProbeError> {
    let Some(avg) = avg else {
        return Err(ProbeError::NoCadence);
    };
    if !cadence_matches(canvas, avg) {
        return Err(ProbeError::Cadence {
            want_num: canvas.num,
            want_den: canvas.den,
            got_num: avg.num,
            got_den: avg.den,
        });
    }
    if let Some(nominal) = nominal {
        if nominal.is_valid() && avg.is_valid() && nominal != avg {
            return Err(ProbeError::Vfr {
                avg_num: avg.num,
                avg_den: avg.den,
                nom_num: nominal.num,
                nom_den: nominal.den,
            });
        }
    }
    Ok(avg)
}

/// What the decoder's pixel format says about alpha, classified by
/// `multiview_ffmpeg::alpha::alpha_support` (the same layout map the decode
/// uses, so the gate can never admit a format the alpha path cannot carry).
/// Kept libav-free here so the decision is unit-tested in the default build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PixelAlpha {
    /// No alpha: an ordinary opaque clip.
    Opaque,
    /// Alpha the NV12+A decode carries (ProRes 4444, qtrle, PNG, TGA, VP9
    /// alpha via `libvpx-vp9`, ...).
    Supported,
    /// Alpha the layout map cannot carry (e.g. paletted `pal8`).
    Unsupported,
}

/// The alpha gate of a runtime clip load (keyers K6, #391; ADR-0058 §3).
///
/// Returns whether the clip's video carries alpha the player will publish as
/// NV12+A. `hevc_layers` is `Some(vps_max_layers)` for an HEVC stream whose
/// VPS could be read, `None` otherwise.
///
/// # Errors
///
/// * [`ProbeError::Alpha`] when the pixel format carries alpha the decode
///   cannot carry.
/// * [`ProbeError::HevcAlpha`] when an HEVC stream declares more than one
///   layer: HEVC alpha is an auxiliary layer libav decodes opaque, and the
///   base layer's pixel format does not reveal it.
pub(super) fn validate_alpha(
    pix_fmt: &str,
    alpha: PixelAlpha,
    hevc_layers: Option<u8>,
) -> Result<bool, ProbeError> {
    if let Some(layers) = hevc_layers.filter(|layers| *layers > 1) {
        return Err(ProbeError::HevcAlpha { layers });
    }
    match alpha {
        PixelAlpha::Opaque => Ok(false),
        PixelAlpha::Supported => Ok(true),
        PixelAlpha::Unsupported => Err(ProbeError::Alpha {
            pix_fmt: pix_fmt.to_owned(),
        }),
    }
}

/// `vps_max_layers_minus1 + 1` of the first HEVC VPS in `extradata`, either an
/// `hvcC` record (first byte `1`) or Annex-B parameter sets; `None` when no
/// VPS is found or the record is malformed. Bounded: walks at most the record's
/// declared arrays, never past `extradata`.
///
/// The VPS header is fixed-length (H.265 §7.3.2.1): after the two-byte NAL
/// header come `vps_video_parameter_set_id` u(4),
/// `vps_base_layer_internal_flag` u(1), `vps_base_layer_available_flag` u(1)
/// and `vps_max_layers_minus1` u(6), so the layer count is in payload bytes 0-1
/// (no emulation-prevention byte can occur there: the NAL header is `0x40`).
#[must_use]
pub(super) fn hevc_vps_max_layers(extradata: &[u8]) -> Option<u8> {
    let vps = if extradata.first() == Some(&1) {
        hvcc_vps(extradata)?
    } else {
        annex_b_vps(extradata)?
    };
    let (&b0, &b1) = (vps.get(2)?, vps.get(3)?);
    Some((((b0 & 0x03) << 4) | (b1 >> 4)) + 1)
}

/// HEVC NAL unit type of a NAL whose first byte is `header`.
const fn hevc_nal_type(header: u8) -> u8 {
    (header >> 1) & 0x3F
}

const HEVC_VPS: u8 = 32;

/// The first VPS NAL of an `hvcC` record (ISO/IEC 14496-15 §8.3.3.1: 22 fixed
/// bytes, `numOfArrays`, then per array a type byte, `numNalus` u16 and
/// `nalUnitLength` u16-prefixed NALs).
fn hvcc_vps(record: &[u8]) -> Option<&[u8]> {
    let arrays = *record.get(22)?;
    let mut at = 23_usize;
    for _ in 0..arrays {
        let kind = *record.get(at)? & 0x3F;
        let count = u16::from_be_bytes([*record.get(at + 1)?, *record.get(at + 2)?]);
        at += 3;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes([*record.get(at)?, *record.get(at + 1)?]));
            let nal = record.get(at + 2..at + 2 + len)?;
            if kind == HEVC_VPS && nal.first().map(|h| hevc_nal_type(*h)) == Some(HEVC_VPS) {
                return Some(nal);
            }
            at += 2 + len;
        }
    }
    None
}

/// The first VPS NAL of Annex-B parameter sets (`00 00 01` start codes).
fn annex_b_vps(bytes: &[u8]) -> Option<&[u8]> {
    let mut i = 0_usize;
    while let Some(window) = bytes.get(i..i + 3) {
        if window == [0, 0, 1] {
            let nal = bytes.get(i + 3..)?;
            if nal.first().map(|h| hevc_nal_type(*h)) == Some(HEVC_VPS) {
                return Some(nal);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

/// Resolve the clip out-point from the declared and probed lengths.
///
/// With both known, the minimum wins. With one known, that one is used. With
/// neither, the load is refused (no guessing).
///
/// # Errors
///
/// [`ProbeError::UnknownLength`] when both are `None`.
pub(super) fn resolve_out_point(
    declared: Option<u64>,
    probed: Option<u64>,
) -> Result<u64, ProbeError> {
    match (declared, probed) {
        (Some(declared), Some(probed)) => Ok(declared.min(probed)),
        (Some(declared), None) => Ok(declared),
        (None, Some(probed)) => Ok(probed),
        (None, None) => Err(ProbeError::UnknownLength),
    }
}

/// Resolve the validated [`PlayoutGeometry`] of a clip from its spec, its probe
/// and the canvas cadence (design 4.2).
///
/// In-point defaults to `0`; the vamp window defaults to the whole trimmed clip
/// when the spec declares none. The geometry constructor validates the nesting
/// and the cadence.
///
/// # Errors
///
/// * Any error [`resolve_out_point`] returns.
/// * [`ProbeError::Window`] when the resolved window is not
///   `in ≤ vamp_in < vamp_out ≤ out`.
pub(super) fn resolve_geometry(
    spec: &ClipSpec,
    probed: &ProbedClip,
    canvas: Rational,
) -> Result<PlayoutGeometry, ProbeError> {
    let out_point = resolve_out_point(spec.out_point, probed.frames)?;
    let in_point = spec.in_point.unwrap_or(0);
    let vamp_in = spec.vamp_in.unwrap_or(in_point);
    let vamp_out = spec.vamp_out.unwrap_or(out_point);
    Ok(PlayoutGeometry::new(
        in_point, out_point, vamp_in, vamp_out, canvas,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::LoadThen;

    fn probed(cadence: Rational, frames: Option<u64>) -> ProbedClip {
        ProbedClip {
            codec: "ffv1".to_owned(),
            width: 64,
            height: 48,
            cadence,
            frames,
            has_audio: false,
            has_alpha: false,
            pix_fmt: "yuv420p".to_owned(),
        }
    }

    #[test]
    fn cadence_mismatch_is_rejected_with_both_rates() {
        let canvas = Rational::new(50, 1);
        let error = validate_cadence(
            canvas,
            Some(Rational::new(25, 1)),
            Some(Rational::new(25, 1)),
        )
        .expect_err("25 fps on a 50 fps canvas is refused");
        assert_eq!(
            error,
            ProbeError::Cadence {
                want_num: 50,
                want_den: 1,
                got_num: 25,
                got_den: 1,
            }
        );
        let message = error.to_string();
        assert!(message.contains("25/1"), "{message}");
        assert!(message.contains("50/1"), "{message}");
    }

    #[test]
    fn ntsc_exact_rational_is_accepted() {
        let canvas = Rational::new(60_000, 1001);
        // The same value written unreduced is still exactly equal.
        let same = validate_cadence(canvas, Some(Rational::new(120_000, 2002)), None)
            .expect("an equal rational is accepted");
        assert_eq!(same, canvas);
        // A nearby decimal rate is a different rate.
        assert!(validate_cadence(canvas, Some(Rational::new(2997, 100)), None).is_err());
        assert!(!cadence_matches(canvas, Rational::new(29_970, 1000)));
    }

    #[test]
    fn vfr_is_rejected() {
        let canvas = Rational::new(25, 1);
        let error = validate_cadence(canvas, Some(canvas), Some(Rational::new(30, 1)))
            .expect_err("average must equal nominal");
        assert_eq!(
            error,
            ProbeError::Vfr {
                avg_num: 25,
                avg_den: 1,
                nom_num: 30,
                nom_den: 1,
            }
        );
    }

    #[test]
    fn alpha_is_rejected() {
        let error = validate_alpha("yuva420p", true).expect_err("alpha is refused");
        assert_eq!(
            error,
            ProbeError::Alpha {
                pix_fmt: "yuva420p".to_owned(),
            }
        );
        assert!(validate_alpha("yuv420p", false).is_ok());
    }

    #[test]
    fn out_point_is_min_of_declared_and_probed() {
        assert_eq!(resolve_out_point(Some(100), Some(48)).unwrap(), 48);
        assert_eq!(resolve_out_point(Some(48), Some(100)).unwrap(), 48);
        assert_eq!(resolve_out_point(Some(48), None).unwrap(), 48);
        assert_eq!(resolve_out_point(None, Some(48)).unwrap(), 48);
    }

    #[test]
    fn unknown_length_without_declared_out_is_refused() {
        assert_eq!(
            resolve_out_point(None, None).expect_err("no length is known"),
            ProbeError::UnknownLength
        );
    }

    #[test]
    fn resolved_geometry_defaults_the_vamp_window_to_the_whole_clip() {
        let canvas = Rational::new(25, 1);
        let spec = ClipSpec::new("/clip.mov", LoadThen::Cue);
        let geometry = resolve_geometry(&spec, &probed(canvas, Some(48)), canvas).unwrap();
        assert_eq!(
            (
                geometry.in_point(),
                geometry.vamp_in(),
                geometry.vamp_out(),
                geometry.out_point(),
            ),
            (0, 0, 48, 48)
        );
        assert_eq!(geometry.cadence(), canvas);
    }
}
