//! # multiview-cli
//!
//! The library half of the `multiview` binary: the argument grammar
//! ([`cli`]), the `validate` subcommand ([`validate`]), the `run`
//! subcommand's headless software engine ([`run`]), and — behind the
//! off-by-default `ffmpeg` feature — the real libav\* end-to-end pipeline
//! (`pipeline`: ingest → composite → encode-once → fan out to file/HLS sinks).
//!
//! The binary ([`crate`]'s `main.rs`) is a thin shell that parses
//! [`cli::Cli`] and dispatches to these modules. Keeping the logic in a library
//! target lets the integration tests under `tests/` exercise the *real* code
//! paths — config validation and the deterministic headless engine — with no
//! process spawning.
//!
//! ## What `run --headless` proves
//!
//! `run --headless` is a pure-software, GPU-free, FFmpeg-free end-to-end smoke
//! of **invariant #1 (output-clock)**: it builds the protected output core
//! ([`multiview_engine`]) wired from a validated layout — the fixed-cadence
//! [`multiview_engine::OutputClock`], the CPU reference [`multiview_engine::CompositorDrive`],
//! and one [`multiview_framestore::TileStore`] per source — attaches built-in
//! test-pattern sources that publish synthetic NV12 frames into those stores,
//! and drives exactly one composited frame per tick for the requested number of
//! ticks. It reports frames produced, the measured cadence, and that the output
//! never faltered (frames == ticks, monotone PTS).
//!
//! ## Isolation (invariant #10)
//!
//! The headless engine consumes engine state through the engine's own outbound
//! [`multiview_engine::EnginePublisher`] (the wait-free latest-state slot + the
//! drop-oldest broadcast). The CLI is a best-effort observer: it never holds a
//! lock the engine needs and never makes the engine `.await` it.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Build-time GPU-capability cross-check + health-warning emit (SA-0 / ADR-0035):
/// detect when a real GPU is present but the wgpu compositor resolved a
/// software/CPU adapter (the silent CPU fallback) and emit a latched, actionable
/// `gpu-present-no-vulkan-adapter` warning through the engine's drop-oldest
/// publisher. A thin seam over the pure hal cross-check + the control emit helper.
pub mod capability_warn;
pub mod cli;
pub mod control;
/// The Conspect entitlement-plane wiring for the cli (CONSPECT-2b/10, ADR-0050):
/// the shared lease store, the published [`licence::WatermarkSignal`] the engine
/// bake samples lock-free (S3), and the sampled [`multiview_licence::EnforcementLevel`]
/// the startup gate (S1) consults. Always compiled — the entitlement *state model*
/// renders consistently regardless of features (ADR-0050 §7).
pub mod licence;
pub mod live_sources;
/// The CONSPECT local-metrics retention feed (engine-seam S5; ADR-0052 §3): an
/// off-hot-loop, read-only subscriber that mirrors live engine events
/// (utilisation / per-input reconnect / incident markers) into the
/// consent-independent [`multiview_telemetry::retention::RetentionStore`].
/// Independent of telemetry consent.
pub mod metrics_retention;
/// Build-capability gating for configured outputs (DEV-B1 / ADR-0044): a
/// `display` output must FAIL a non-`display-kms` build with a clear error —
/// never be silently skipped. Always compiled, so the default build tests the
/// rejection path and a `display-kms` build tests the acceptance path.
pub mod outputs;
pub mod preview;
pub mod run;
pub mod system_metrics;
pub mod validate;

/// The overlay draw-data baker (feature `overlay`): builds the per-frame overlay
/// primitives — clock label, dB meter, safe-area, tally, burned-in subtitles —
/// that the run paths bake into the composited program off the hot path.
#[cfg(feature = "overlay")]
pub mod overlays;

/// The **real wall-clock time-of-day source** for the on-screen clock overlay
/// (feature `overlay`): reads the OS `CLOCK_REALTIME` via `std` (no NTP
/// reimplementation), exposes an injectable [`wallclock::WallClock`] seam for
/// tests, and carries the [`multiview_overlay::clock::TimeRef`] reference badge. The
/// displayed time-of-day is sampled live at bake time (anti-drift), never derived
/// from the output-tick counter — the engine's output cadence stays untouched.
#[cfg(feature = "overlay")]
pub mod wallclock;

/// In-process **synthetic video sources** (ADR-0027): colour bars, a solid
/// colour, and a full-frame clock — rendered to NV12 and published into a
/// `TileStore` like any decoded feed. `bars`/`solid` build everywhere; the clock
/// renderer needs the `overlay` feature.
pub mod synth;

/// The full libav\* end-to-end `multiview run` pipeline (ingest → composite →
/// encode-once → fan out to file/HLS sinks). Behind the off-by-default `ffmpeg`
/// feature so the baseline build stays pure-Rust; software H.264/H.265 needs
/// `gpl-codecs` on top.
#[cfg(feature = "ffmpeg")]
pub mod pipeline;

/// Per-source **runtime audio ingest** (AUD-2): the audio peer of the video
/// decode thread. Opens + decodes each file/URL source's audio on its own
/// thread into a lock-free per-source `AudioStore` (canonical 48 kHz stereo),
/// under the same supervised-reconnect bracket the video ingest uses. The output
/// clock samples those stores via the `ProgramBus` per tick — never paced or
/// stalled by a source (invariants #1/#10). Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod audio;

/// Native in-pipeline **HLS WebVTT caption ingest**: resolve a source's subtitle
/// rendition from its HLS master playlist, demux + decode it on an isolated
/// reader thread, and publish cues into a per-source store the overlay baker
/// samples per output tick (per-tile burn-in). Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod captions;
