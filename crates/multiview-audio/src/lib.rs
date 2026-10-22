//! # multiview-audio
//!
//! Per-input audio decode/resample/mix/route (program bus + discrete tracks)
//! and **EBU R128 / ITU-R BS.1770 loudness metering**.
//!
//! This crate's *default* build is pure Rust with no native dependencies. It
//! provides:
//!
//! - [`loudness`] — the BS.1770-4 measurement chain: K-weighting pre-filter +
//!   RLB filter, mean-square integration, channel weighting, absolute/relative
//!   gating, and momentary / short-term / integrated loudness + loudness range
//!   (LRA), plus an oversampled true-peak (dBTP) estimate.
//! - [`loudnorm`] — live EBU R128 loudness **normalisation** of the program bus
//!   (AUD-6): a smoothed makeup gain toward a target LUFS (−23 broadcast / −16
//!   web) driven off [`loudness`]'s short-term measurement, with a true-peak
//!   limiter (−1.5 dBTP ceiling) so normalisation never clips, a −70 LUFS gate so
//!   a lost input's silence is not amplified, and discrete tracks left unaltered
//!   (ADR-R005/R006).
//! - [`ballistics`] — selectable meter ballistics: PPM Type I/IIa/IIb (IEC
//!   60268-10), VU, sample-peak (IEC TR 60268-18) and true-peak (ITU-R BS.1770),
//!   each with the standardised integration/decay constants.
//! - [`correlation`] — stereo phase-correlation meter, goniometer (Lissajous)
//!   points, and ITU-R BS.775 Lo/Ro + Lt/Rt surround-downmix metering.
//! - [`meterdata`] — the meter → overlay draw-data bridge: sample the meters
//!   read-only and **conflate** them to ~30 Hz for the on-screen meters/scopes
//!   (the dB→deflection mapping itself lives in `multiview-compositor`).
//! - [`chanmap`] — channel mapping / shuffle / de-embed routing matrix for
//!   multichannel (16+) streams.
//! - [`capability`] — the machine-readable per-output audio **capability
//!   matrix** (TS/RTSP = N tracks, HLS = select-one, RTMP = endpoint-gated,
//!   NDI = channel-map): the declarative half of AUD-7's validation, referencing
//!   [`ChannelLayout`]. The routing *schema* it gates lives in `multiview-config`.
//! - [`probe`] — content-aware audio fault probes (silence, over-level, clip,
//!   phase-invert, imbalance) with dwell/hysteresis, emitting
//!   [`multiview_core::alarm`] records.
//! - [`mixer`] — the mix/route *model*: a program bus, clean discrete
//!   per-input tracks, and a per-input gain/route matrix (ADR-R005).
//! - [`store`] — the bounded, lock-free, gap-free per-source **last-good audio
//!   store** (the audio peer of the video tile store) plus the per-source audio
//!   decode loop: a decode thread publishes blocks, the output clock samples
//!   exactly the frames it needs per tick (never pacing, never blocking — #1/#10).
//! - [`adaptive`] — a ratio-driven (varying ±ppm) linear-interpolating
//!   resampler the HDMI display-audio servo drives to track the scanout clock
//!   (the mpv/Kodi "display-resample" technique); pure Rust, the runtime half of
//!   the resample machinery whose fixed-ratio decode-time half lives in
//!   [`decode`].
//! - [`filter`] / [`truepeak`] — the supporting DSP primitives.
//!
//! Decode and resample (turning coded packets into the in-memory
//! [`AudioBlock`]s these APIs operate on) live in the `decode` module, behind
//! the off-by-default `ffmpeg` feature, and are **not** part of the pure-Rust
//! layer. That module calls only `multiview-ffmpeg`'s safe wrappers — this crate
//! never touches libav directly and stays `unsafe_code = forbid`.
//!
//! Per ADR-R006 the meter is designed to run **read-only and off the hot
//! path**: tap audio into a meter on a separate thread and never let it
//! back-pressure the engine.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod adaptive;
pub mod ballistics;
pub mod cadence;
pub mod capability;
pub mod chanmap;
pub mod correlation;
#[cfg(feature = "ffmpeg")]
pub mod decode;
pub mod error;
pub mod filter;
pub mod format;
pub mod loopdeck;
pub mod loudness;
pub mod loudnorm;
pub mod meterdata;
pub mod mixer;
pub mod probe;
pub mod program;
pub mod store;
pub mod tone;
pub mod truepeak;

pub use adaptive::{AdaptiveResampler, RatioPpm};
pub use ballistics::{Ballistics, MeterScale, PeakMode, PpmKind, SampleScale};
pub use capability::{DiscreteTracks, OutputCapability, OutputTransport, TrackSupport};
pub use chanmap::{ChannelMatrix, Route};
pub use correlation::{CorrelationMeter, GonioPoint, SurroundDownmix};
#[cfg(feature = "ffmpeg")]
pub use decode::{meter_file, AudioFileDecoder, DecodedBlock};
pub use error::{AudioError, Result};
pub use format::{AudioBlock, AudioFormat, ChannelLayout};
pub use loopdeck::{LoopDeck, LoopDeckError, DEFAULT_CROSSFADE_FRAMES};
pub use loudness::LoudnessMeter;
pub use loudnorm::{
    LoudnessTarget, LoudnormProcessor, DEFAULT_TRUE_PEAK_CEILING_DBTP, LIVE_TOLERANCE_LU,
};
pub use meterdata::{Conflator, MeterSample, StereoMeterSample, DISPLAY_HZ};
pub use mixer::{GainRamp, Mixer, RoutePoint};
pub use probe::{AudioProbeBank, AudioProbeConfig, ProbeSeverityProfile};
pub use program::{ApplyClass, ProgramBus, SwitchTier};
#[cfg(feature = "ffmpeg")]
pub use store::audio_decode_loop;
pub use store::AudioStore;
pub use tone::{line_up_tone_amplitude, ToneGenerator, LINE_UP_TONE_PEAK_DBFS, REFERENCE_TONE_HZ};
