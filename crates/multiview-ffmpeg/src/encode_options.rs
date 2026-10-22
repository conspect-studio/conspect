//! Typed, libav-free encoder-`AVOption` surface + the fixed preview-encode
//! option sets (ADR-P006).
//!
//! [`CodecOptions`] is the encoder-side sibling of
//! [`MuxOptions`](crate::mux_options::MuxOptions): an ordered, validated set of
//! `(key, value)` `AVOption` pairs a caller hands to
//! `VideoEncoder::new_with_options` / `AudioEncoder::new_with_options`, which
//! turn it into a libav dictionary for `avcodec_open2`. The model is pure so it
//! compiles and unit-tests in the default (no-`ffmpeg`) build, and it validates
//! up front that no key/value carries an interior NUL (which could never become
//! a C string for `av_dict_set`).
//!
//! ## The fixed preview profile (ADR-P006)
//!
//! Preview WHEP encodes are fixed policy, not knobs: **zerolatency-class rate
//! control**, **B-frames hard-off** (RTP carries no DTS — decode order must be
//! transmission order), **repeat-headers** (SPS/PPS with every IDR so a
//! late-joining viewer decodes from the next keyframe), and a **2-second GOP**.
//! [`preview_h264_options`] emits that set for a *selected* H.264 encoder name
//! (NVENC / VAAPI / VideoToolbox / libx264 families); [`preview_vp8_options`]
//! is the `libvpx` software-rung equivalent (realtime deadline, zero lag,
//! error-resilient — VP8 has no B-frame concept, so no `bf` pair).
//!
//! **Repeat-headers is structural, not an option pair:** the crate's encoders
//! never set `AV_CODEC_FLAG_GLOBAL_HEADER`, and the Annex-B H.264 encoders
//! (NVENC's `repeatSPSPPS`, x264, VAAPI's packed headers) emit in-band SPS/PPS
//! at every IDR exactly when that flag is absent. Where a family has an
//! explicit forced-IDR knob (`forced-idr` on NVENC/libx264) it is set so the
//! [`force_next_keyframe`](crate::encode::VideoEncoder::force_next_keyframe)
//! seam (ADR-0049) produces true IDRs, not open-GOP I-frames.
//!
//! The GOP length comes from the exact preview cadence rational — never a
//! float fps (invariant #3).

use multiview_core::time::Rational;

/// An ordered, validated set of encoder `AVOption` key/value pairs to apply at
/// `avcodec_open2`.
///
/// Mirrors [`MuxOptions`](crate::mux_options::MuxOptions): order is preserved
/// (a stable, deterministic surface for tests and logs) and both sides of each
/// pair are validated NUL-free up front.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodecOptions {
    pairs: Vec<(String, String)>,
}

/// An option key or value that cannot be expressed as a libav option (it
/// carries an interior NUL byte, so it could never become a C string for
/// `av_dict_set`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("codec option {field} {text:?} contains an interior NUL byte")]
pub struct CodecOptionError {
    /// Which side of the pair was malformed (`"key"` or `"value"`).
    field: &'static str,
    /// The malformed text (for diagnostics).
    text: String,
}

impl CodecOptions {
    /// An empty option set (equivalent to opening with no dictionary).
    #[must_use]
    pub const fn new() -> Self {
        Self { pairs: Vec::new() }
    }

    /// Build from an arbitrary slice of `(key, value)` pairs, validating that
    /// neither side carries an interior NUL.
    ///
    /// # Errors
    /// Returns [`CodecOptionError`] if any key or value contains a `\0`.
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Result<Self, CodecOptionError> {
        let mut out = Self::new();
        for &(key, value) in pairs {
            out = out.try_set(key, value)?;
        }
        Ok(out)
    }

    /// Set one option, validating both sides for an interior NUL.
    ///
    /// # Errors
    /// Returns [`CodecOptionError`] if `key` or `value` contains a `\0`.
    pub fn try_set(mut self, key: &str, value: &str) -> Result<Self, CodecOptionError> {
        if key.contains('\0') {
            return Err(CodecOptionError {
                field: "key",
                text: key.to_owned(),
            });
        }
        if value.contains('\0') {
            return Err(CodecOptionError {
                field: "value",
                text: value.to_owned(),
            });
        }
        self.pairs.push((key.to_owned(), value.to_owned()));
        Ok(self)
    }

    /// Append a known-good literal pair (crate-internal builder for the fixed
    /// option sets below; the literals carry no NUL by construction).
    fn push_literal(mut self, key: &str, value: &str) -> Self {
        self.pairs.push((key.to_owned(), value.to_owned()));
        self
    }

    /// The ordered, validated key/value pairs.
    #[must_use]
    pub fn as_pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    /// Whether any option is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

/// The 2-second preview GOP length in frames for an exact cadence rational,
/// rounded to the nearest frame and clamped to at least 1.
///
/// `gop = round(2 * fps)` computed in exact integer arithmetic (invariant #3:
/// never float fps). A degenerate rate (zero/negative numerator or
/// denominator) yields 1 — `0` would mean "codec default" downstream, which is
/// never what the fixed preview profile wants.
#[must_use]
pub fn preview_gop_frames(fps: Rational) -> u32 {
    if fps.num <= 0 || fps.den <= 0 {
        return 1;
    }
    // round(2*num/den) = floor((4*num + den) / (2*den)), all positive here.
    let rounded = fps
        .num
        .saturating_mul(4)
        .saturating_add(fps.den)
        .checked_div(fps.den.saturating_mul(2))
        .unwrap_or(1);
    u32::try_from(rounded.max(1)).unwrap_or(1)
}

/// The fixed ADR-P006 preview option set for a selected H.264 encoder name.
///
/// Always emits the codec-generic pairs first — `g` (the 2 s GOP from the
/// exact `fps` rational) and `bf=0` (B-frames hard-off) — then the
/// family-specific zerolatency-class knobs, keyed on the encoder-name suffix
/// the same way the NVENC device pin is
/// ([`VideoEncodeTarget::cuda_device`](crate::encode::VideoEncodeTarget)):
///
/// * `*_nvenc` — `tune=ull`, `zerolatency=1`, `delay=0` (no reorder buffer),
///   `rc=cbr`, `forced-idr=1`.
/// * `*_vaapi` — `rc_mode=CBR`, `async_depth=1` (no pipelined latency).
/// * `*_videotoolbox` — `realtime=1`, `prio_speed=1`.
/// * `libx264` (`gpl-codecs` builds) — `tune=zerolatency`, `forced-idr=1`.
/// * anything else — the generic pairs only (never a family knob that could
///   fail an unknown encoder's open).
///
/// Repeat-headers is structural (no `GLOBAL_HEADER` flag) — see the
/// [module docs](self).
#[must_use]
pub fn preview_h264_options(encoder_name: &str, fps: Rational) -> CodecOptions {
    let gop = preview_gop_frames(fps).to_string();
    let base = CodecOptions::new()
        .push_literal("g", &gop)
        .push_literal("bf", "0");
    if encoder_name.ends_with("_nvenc") {
        base.push_literal("tune", "ull")
            .push_literal("zerolatency", "1")
            .push_literal("delay", "0")
            .push_literal("rc", "cbr")
            .push_literal("forced-idr", "1")
    } else if encoder_name.ends_with("_vaapi") {
        base.push_literal("rc_mode", "CBR")
            .push_literal("async_depth", "1")
    } else if encoder_name.ends_with("_videotoolbox") {
        base.push_literal("realtime", "1")
            .push_literal("prio_speed", "1")
    } else if encoder_name == "libx264" {
        base.push_literal("tune", "zerolatency")
            .push_literal("forced-idr", "1")
    } else {
        base
    }
}

/// The fixed ADR-P006 preview option set for the `libvpx` VP8 software rung.
///
/// Zerolatency-class realtime config: `deadline=realtime` + `cpu-used=8`
/// (speed over quality at preview bitrates), `lag-in-frames=0` (no lookahead —
/// 1-in-1-out, keyframes forcible on the very next frame), and
/// `error-resilient=default` (frame-loss resilience for the lossy WebRTC
/// path). The 2 s GOP rides the generic `g`; VP8 has no B-frame concept, so
/// there is no `bf` pair.
#[must_use]
pub fn preview_vp8_options(fps: Rational) -> CodecOptions {
    let gop = preview_gop_frames(fps).to_string();
    CodecOptions::new()
        .push_literal("g", &gop)
        .push_literal("deadline", "realtime")
        .push_literal("cpu-used", "8")
        .push_literal("lag-in-frames", "0")
        .push_literal("error-resilient", "default")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{preview_gop_frames, preview_h264_options, CodecOptions};
    use multiview_core::time::Rational;

    #[test]
    fn gop_rounds_half_up_for_ntsc_rates() {
        // 2 s at 59.94 fps = 119.88 frames -> 120 (round to nearest, exact
        // integer arithmetic; the floor would be 119).
        assert_eq!(preview_gop_frames(Rational::FPS_59_94), 120);
    }

    #[test]
    fn nvenc_suffix_matches_hevc_variant_too() {
        // The family key is the `_nvenc` suffix, so a future hevc_nvenc preview
        // rung inherits the same zerolatency set without a new mapping.
        let opts = preview_h264_options("hevc_nvenc", Rational::new(15, 1));
        assert!(opts
            .as_pairs()
            .iter()
            .any(|(k, v)| k == "tune" && v == "ull"));
    }

    #[test]
    fn options_round_trip_their_pairs_in_order() {
        let opts = CodecOptions::new()
            .try_set("a", "1")
            .unwrap()
            .try_set("b", "2")
            .unwrap();
        assert_eq!(
            opts.as_pairs(),
            &[
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "2".to_owned())
            ]
        );
    }
}
