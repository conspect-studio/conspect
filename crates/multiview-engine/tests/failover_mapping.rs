//! The single shared `FailoverSlate` config policy maps onto BOTH render paths
//! identically — the layout-tile slate image AND the passthrough program's
//! pre-baked output slate kind. This is the seam that makes "configurable the
//! same way" a fact: one enum, two render targets, one mapping each.
//!
//! The output-slate mapping (`output_slate_kind` / `output_slate_audio`) is
//! behind the engine's `ffmpeg` feature (the output slate baker is libav-backed),
//! so this whole file is `ffmpeg`-gated; the pure layout-tile image path is also
//! exercised by `tests/failover_slate.rs` in the default build.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::pipeline::CanvasColor;
use multiview_config::FailoverSlate;
use multiview_engine::slate::{failover_slate_image, output_slate_audio, output_slate_kind};
use multiview_output::slate::{SlateAudio, SlateKind};

#[test]
fn failover_maps_to_the_output_slate_kind() {
    // The passthrough/transcode program's pre-baked slate (multiview-output)
    // honours the SAME policy as a layout tile via this mapping.
    assert_eq!(output_slate_kind(FailoverSlate::Bars), SlateKind::SmpteBars);
    assert_eq!(
        output_slate_kind(FailoverSlate::NoSignal),
        SlateKind::NoSignal
    );
    assert_eq!(output_slate_kind(FailoverSlate::Black), SlateKind::Black);
}

#[test]
fn bars_carries_the_one_khz_tone_companion_others_are_silent() {
    // The 1 kHz tone is the audio companion to bars (the broadcast "we have a
    // problem" signal). It rides the Bars policy; NoSignal/Black are silent.
    // (The tone only flows once the run-side audio path is wired — this is the
    // policy's audio selection, not a fabricated tone.)
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
fn failover_builds_a_distinct_tile_image_per_choice() {
    // The layout-tile half of the same policy: each choice builds a distinct NV12
    // slate image at canvas size, reused per tick by the drive.
    let (w, h) = (256, 64);
    let canvas = CanvasColor::default();

    let bars = failover_slate_image(FailoverSlate::Bars, w, h, canvas).unwrap();
    let black = failover_slate_image(FailoverSlate::Black, w, h, canvas).unwrap();
    let nosignal = failover_slate_image(FailoverSlate::NoSignal, w, h, canvas).unwrap();

    assert_eq!(bars.width(), w);
    assert_eq!(bars.height(), h);

    // Black is a flat limited-range black (luma 16, neutral chroma).
    assert_eq!(black.sample(w / 2, h / 2).unwrap(), (16, 128, 128));

    // Bars are a multi-band staircase (left brighter than right; not flat).
    let (bl, _, _) = bars.sample(4, 32).unwrap();
    let (br, _, _) = bars.sample(w - 4, 32).unwrap();
    assert!(bl > br, "bars descend in luma left->right");

    // NoSignal is a distinct card (not the bars staircase, not pure black).
    let (nl, _, _) = nosignal.sample(w / 2, h / 2).unwrap();
    assert_ne!(nl, 16, "the NoSignal card is distinct from pure black");
    let nosignal_uniform = (0..w).all(|x| nosignal.sample(x, 32).map(|s| s.0) == Some(nl));
    assert!(
        nosignal_uniform,
        "the NoSignal card is a flat field, not bars"
    );
}
