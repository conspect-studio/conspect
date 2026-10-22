//! ALSA HDMI/DisplayPort audio for the display sink (DEV-B4, [ADR-0044] §5,
//! brief `docs/research/display-out.md` §5).
//!
//! The display sink ([`super`]) scans the composited canvas out to glass; this
//! module adds the **audio** half for a display head: ALSA-direct PCM to the
//! HDMI/DP sink, ELD-gated, reconciling three independent clocks.
//!
//! ## The three-clock problem
//!
//! The engine tick (invariant #1), the display **pixel** clock (observed via the
//! flip timestamps the sink exports), and the ALSA **sample** clock are three
//! free-running crystals. The sink reconciles them with a **bounded drop-oldest
//! FIFO** ([`AudioFifo`]) feeding the PCM, plus a **buffer-level servo**
//! ([`BufferServo`]) that drives multiview-audio's ratio-varying
//! [`AdaptiveResampler`](multiview_audio::AdaptiveResampler) so the audio rate
//! tracks the *scanout* clock — the mpv/Kodi "display-resample" technique. The
//! resampler is **not new machinery**: `multiview-audio` already owns resampling
//! (ADR-R005); the servo only varies its ratio within a clamped ±ppm band.
//!
//! ## ELD gating
//!
//! HDMI audio rides data islands in the video stream, so it flows only while the
//! pipe is lit — which our always-lit sink guarantees. The driver publishes the
//! sink's audio capability as ELD at `/proc/asound/cardN/eld#C.P`; [`parse_eld`]
//! turns those bytes into an [`EldCapability`]. An **EDID-less head has no ELD
//! and therefore no audio path** — the sink stays silent and alive (the picture
//! is unaffected), never a panic.
//!
//! ## Isolation (invariants #1 + #10)
//!
//! The sink is a pure **consumer**: the engine holds only a
//! [`DisplayAudioPublisher`] — a bounded, never-blocking-on-the-device push
//! (one short mutex-guarded in-memory copy into the drop-oldest FIFO; the
//! drain thread holds that lock only for in-memory copies, never across a PCM
//! call). A wedged/silent device drops audio (bounded FIFO, never grows) and
//! the engine never notices — the audio sink never paces the tick.
//!
//! ## Hardware isolation for CI
//!
//! Everything here is **pure Rust, always compiled, and CI-tested without
//! hardware** over two trait seams — [`EldSource`] and [`AlsaSink`] — exercised
//! by scripted mocks: ELD parsing, the FIFO drop bound, the servo loop, and the
//! [`XrunRecovery`] machine. The real `/proc/asound` reader and the libasound
//! PCM live in [`alsa`] behind the off-by-default `display-kms` feature and run
//! only on hardware (the t630 / Raspberry Pi node legs).
//!
//! [ADR-0044]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0044.md

pub mod discover;
pub mod eld;
pub mod fifo;
pub mod pcm_format;
pub mod servo;
pub mod sink;
pub mod tracker;
pub mod xrun;

/// The real libasound PCM + `/proc/asound`/ELD-control reader (feature
/// `display-kms`). Everything that talks to libasound or reads `/proc` lives
/// here; its hardware paths run only on hardware (CI covers the pure seam
/// above via mocks).
#[cfg(feature = "display-kms")]
pub mod alsa;

pub use discover::{pick_eld_entry, vc4_card_candidates};
pub use eld::{parse_eld, parse_proc_eld_text, EldCapability};
pub use fifo::AudioFifo;
pub use pcm_format::{
    f32_to_s16, f32_to_s24, f32_to_s32, negotiate_sample_format, PcmSampleFormat,
};
pub use servo::{drain_ratio, skew_ms, BufferServo};
pub use sink::{
    AlsaSink, AudioStatsSnapshot, DisplayAudioConfig, DisplayAudioPublisher, DisplayAudioSink,
    EldSource, FlipClock, PcmParams,
};
pub use tracker::SkewTracker;
pub use xrun::{PcmOutcome, RecoverAction, XrunRecovery, XrunState};
