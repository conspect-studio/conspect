//! The configurable **failover slate** — mapping the shared
//! [`multiview_config::FailoverSlate`] policy onto BOTH render paths (ADR-0027 /
//! ADR-0030).
//!
//! A single config field — `on_loss` on a layout cell *or* on a program — picks
//! what is shown when a source is lost or misbehaving. This module is the seam
//! that makes that one policy drive both render paths identically:
//!
//! * [`failover_slate_image`] builds the **layout-tile** slate: one tagged NV12
//!   [`Nv12Image`] at canvas size that the compositor drive
//!   ([`crate::CompositorDrive`]) composites for a down cell, scaled-at-composite
//!   into the cell (RT-6). Each distinct slate is built **once** and reused per
//!   tick (invariant #1 — no per-tick slate allocation on the output clock).
//! * [`output_slate_kind`] / [`output_slate_audio`] map the policy onto the
//!   **passthrough / transcode** program's pre-baked
//!   [`multiview_output::slate::SlateKind`] / [`SlateAudio`] (the GP-4 slate of
//!   ADR-0030 §4), so a non-layout program honours the **same** `on_loss` choice.
//!
//! The 1 kHz **tone** companion to `Bars` is selected here ([`output_slate_audio`]
//! returns [`SlateAudio::Tone1k`] for `Bars`); it is emitted by the run-side
//! audio path once that path carries audio (it is *not* fabricated by the video
//! slate). The other policies select [`SlateAudio::Silence`].

use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
#[cfg(feature = "ffmpeg")]
use multiview_output::slate::{SlateAudio, SlateKind};

pub use multiview_config::FailoverSlate;

use crate::error::{Error, Result};

/// The limited-range luma of the **black** failover slate (NV12, neutral chroma).
const BLACK_Y: u8 = 16;
/// The neutral chroma code value (`Cb`/`Cr`) for the black slate.
const NEUTRAL_C: u8 = 128;

/// The "no-signal blue" card, as an 8-bit sRGB-ish `(r, g, b)` converted into the
/// canvas output space (the same deep blue the program-level
/// [`FailoverSlate::NoSignal`] slate uses in `multiview-output`). Distinct from
/// black and from bars.
const NOSIGNAL_RGB: (u8, u8, u8) = (0, 16, 80);

/// Build the **layout-tile** failover slate for `slate` at `width`x`height` in
/// the canvas output space (invariant #8 — tagged with the canvas's output
/// `ColorInfo`).
///
/// * [`FailoverSlate::Bars`] → SMPTE/EBU 75 % colour bars
///   ([`Nv12Image::color_bars`]) — the line-up signal (ADR-0027).
/// * [`FailoverSlate::NoSignal`] → the recognisable signal-lost card (a deep
///   "no-signal blue" field).
/// * [`FailoverSlate::Black`] → a full-frame limited-range black raster.
///
/// Built **once** per distinct policy by [`crate::CompositorDrive`] and reused
/// every tick (scaled-at-composite into the cell rect, RT-6), so the protected
/// output clock does no per-tick slate work (invariant #1).
///
/// # Errors
///
/// Returns [`Error::Canvas`] for non-positive/odd dimensions, or a canvas colour
/// axis with no CPU implementation (the compositor surfaces the typed error).
pub fn failover_slate_image(
    slate: FailoverSlate,
    width: u32,
    height: u32,
    canvas: CanvasColor,
) -> Result<Nv12Image> {
    let result = match slate {
        FailoverSlate::NoSignal => {
            let (r, g, b) = NOSIGNAL_RGB;
            Nv12Image::solid_rgb(width, height, r, g, b, canvas)
        }
        FailoverSlate::Black => Nv12Image::solid(
            width,
            height,
            BLACK_Y,
            NEUTRAL_C,
            NEUTRAL_C,
            canvas.output_tag(),
        ),
        // `Bars` is the broadcast standard; `FailoverSlate` is `#[non_exhaustive]`,
        // so a future picture also falls back to bars rather than panicking on the
        // output clock.
        FailoverSlate::Bars | _ => Nv12Image::color_bars(width, height, canvas),
    };
    result.map_err(|e| Error::Canvas(e.to_string()))
}

/// Map the shared failover policy onto the **passthrough / transcode** program's
/// pre-baked output slate picture ([`SlateKind`], ADR-0030 §4 GP-4).
///
/// This is the seam that makes "configurable the same way" a fact: the SAME
/// [`FailoverSlate`] that selects a layout tile's slate ([`failover_slate_image`])
/// selects the program-level slate the encoder-less splice replays on input loss.
///
/// Behind the `ffmpeg` feature: the output slate baker ([`SlateKind`]) lives in
/// `multiview-output`'s `ffmpeg`-gated `slate` module (it bakes a real coded
/// slate via libav).
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn output_slate_kind(slate: FailoverSlate) -> SlateKind {
    match slate {
        FailoverSlate::NoSignal => SlateKind::NoSignal,
        FailoverSlate::Black => SlateKind::Black,
        // `Bars` is the broadcast standard; a future (`#[non_exhaustive]`) picture
        // also maps to bars (matches the tile default).
        FailoverSlate::Bars | _ => SlateKind::SmpteBars,
    }
}

/// The audio companion to a failover policy for the passthrough / transcode
/// program's pre-baked slate ([`SlateAudio`], ADR-0030 §4).
///
/// [`FailoverSlate::Bars`] carries the broadcast **1 kHz tone**; every other
/// picture is **silent**. The tone is baked by `multiview-output` only when the
/// program carries audio — this function expresses the *policy*, it does not
/// fabricate a tone where no audio flows.
///
/// Behind the `ffmpeg` feature ([`SlateAudio`] is `ffmpeg`-gated in
/// `multiview-output`).
#[cfg(feature = "ffmpeg")]
#[must_use]
pub fn output_slate_audio(slate: FailoverSlate) -> SlateAudio {
    match slate {
        FailoverSlate::Bars => SlateAudio::Tone1k,
        // NoSignal / Black (and any future picture) are silent.
        _ => SlateAudio::Silence,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn output_kind_mapping_is_total_and_distinct() {
        assert_eq!(output_slate_kind(FailoverSlate::Bars), SlateKind::SmpteBars);
        assert_eq!(
            output_slate_kind(FailoverSlate::NoSignal),
            SlateKind::NoSignal
        );
        assert_eq!(output_slate_kind(FailoverSlate::Black), SlateKind::Black);
    }

    #[cfg(feature = "ffmpeg")]
    #[test]
    fn only_bars_carries_the_tone() {
        assert_eq!(output_slate_audio(FailoverSlate::Bars), SlateAudio::Tone1k);
        assert_eq!(
            output_slate_audio(FailoverSlate::NoSignal),
            SlateAudio::Silence
        );
        assert_eq!(
            output_slate_audio(FailoverSlate::Black),
            SlateAudio::Silence
        );
    }

    #[test]
    fn black_image_is_limited_range_black() {
        let img =
            failover_slate_image(FailoverSlate::Black, 16, 16, CanvasColor::default()).unwrap();
        assert_eq!(img.sample(8, 8).unwrap(), (BLACK_Y, NEUTRAL_C, NEUTRAL_C));
    }
}
