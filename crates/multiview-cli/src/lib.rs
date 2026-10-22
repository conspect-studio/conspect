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

/// Default capacity of the process log-tail ring (ADR-0060 §4.4): the last
/// ~2000 structured records are retained for `GET /api/v1/logs`, drop-oldest.
pub const LOG_RING_CAPACITY: usize = 2000;

/// The process-global log-tail ring, set once by the binary's `init_tracing`
/// (which installs the telemetry capture layer feeding the same `Arc`) and read
/// by the control-plane wiring ([`log_ring`]). A `OnceLock` so the singleton is
/// established at startup before any subcommand runs. Lives in the **library**
/// (not the binary) so [`control`] can read it in both the binary and the lib's
/// integration tests. Bounded drop-oldest, read-only — it can never
/// back-pressure the engine (invariant #10).
static LOG_RING: std::sync::OnceLock<std::sync::Arc<multiview_telemetry::LogRing>> =
    std::sync::OnceLock::new();

/// Publish the process log-tail ring (set-once). The binary calls this from
/// `init_tracing` with the same `Arc` the telemetry capture layer feeds; a
/// redundant set is ignored. Returns whether this call established the ring.
pub fn set_log_ring(ring: std::sync::Arc<multiview_telemetry::LogRing>) -> bool {
    LOG_RING.set(ring).is_ok()
}

/// The process log-tail ring the capture layer feeds, if logging was
/// initialized. [`control::bind_and_serve`] wires this into the `AppState` so
/// `GET /api/v1/logs` serves live captured records; `None` (no capture
/// installed) leaves the `AppState`'s empty default ring.
#[must_use]
pub fn log_ring() -> Option<std::sync::Arc<multiview_telemetry::LogRing>> {
    LOG_RING.get().map(std::sync::Arc::clone)
}

/// Build-time GPU-capability cross-check + health-warning emit (SA-0 / ADR-0035):
/// detect when a real GPU is present but the wgpu compositor resolved a
/// software/CPU adapter (the silent CPU fallback) and emit a latched, actionable
/// `gpu-present-no-vulkan-adapter` warning through the engine's drop-oldest
/// publisher. A thin seam over the pure hal cross-check + the control emit helper.
pub mod boot;
pub mod capability_warn;
pub mod cli;
/// Config-file watch (ADR-W020): hot-reload the impacted parts of the boot
/// config when the file changes externally — through the SAME apply machinery
/// the Web/API uses; an invalid file changes nothing (warn + health event).
pub mod config_watch;
pub mod control;
/// The Conspect entitlement-plane wiring for the cli (CONSPECT-2b/10, ADR-0050):
/// the shared lease store, the published [`licence::WatermarkSignal`] the engine
/// bake samples lock-free (S3), and the sampled [`multiview_licence::EnforcementLevel`]
/// the startup gate (S1) consults. Always compiled — the entitlement *state model*
/// renders consistently regardless of features (ADR-0050 §7).
pub mod licence;
pub mod live_overlays;
pub mod live_sources;
/// Off-hot-path **program-bus loudness telemetry** (AUD-8): a read-only EBU R128
/// compliance meter that taps the emitted (post-loudnorm) program audio and
/// pushes a conflated [`multiview_events::AudioLoudness`] sample (M/S/I/LRA/dBTP +
/// compliance reference) onto the engine's drop-oldest event stream at ~10 Hz, so
/// the WebUI loudness meter lights up. Read-only and non-blocking — it can never
/// back-pressure the engine (inv #10). Always compiled (the pure meter is
/// GPU/FFmpeg-free); the bake-consumer wiring lives behind the `ffmpeg` pipeline.
pub mod loudness_telemetry;
/// The CONSPECT local-metrics retention feed (engine-seam S5; ADR-0052 §3): an
/// off-hot-loop, read-only subscriber that mirrors live engine events
/// (utilisation / per-input reconnect / incident markers) into the
/// consent-independent [`multiview_telemetry::retention::RetentionStore`].
/// Independent of telemetry consent.
pub mod metrics_retention;
/// The `multiview node` display-node subcommand (DEV-B5 / ADR-0045): the
/// software bootstrap — node config parse/validate, the keypair-bound
/// enrollment request + display pairing code, the node clock-mode policy, and
/// the §8 presentation frame chooser. Always compiled and CI-tested; the live
/// ingest → scanout path is a hardware follow-on.
pub mod node;
/// Build-capability gating for configured outputs (DEV-B1 / ADR-0044): a
/// `display` output must FAIL a non-`display-kms` build with a clear error —
/// never be silently skipped. Always compiled, so the default build tests the
/// rejection path and a `display-kms` build tests the acceptance path.
pub mod outputs;
/// The live adaptive-placement execution loop (GPU-5c): the off-hot-path
/// `LoadPoller → arc_swap` poll + `PlacementController` observe/dispatch loop
/// that activates GPU-5b's inert controller, records the placement counters,
/// emits `ShedLoad`, and drives the engine make-before-break crosspoint.
pub mod placement;
pub mod preview;
pub mod run;
/// Map the HAL probe + compiled features + resolved compositor adapter onto the
/// [`multiview_control::system::SystemCapabilities`] DTO for
/// `GET /api/v1/system/capabilities` (ADR-W030). A static startup snapshot
/// (invariant #10).
pub mod system_capabilities;
pub mod system_metrics;
/// Live WHEP preview egress provider (ADR-P006), gated behind `webrtc-native`:
/// wires the native `multiview-webrtc` `WhepEgress` into the control plane so a
/// browser can WHEP-play a preview tap over real DTLS/SRTP, with audio, on all
/// scopes. Strictly isolated (invariant #10): it samples wait-free taps and
/// pushes into bounded drop-oldest feeds; the driver never awaits a client.
#[cfg(feature = "webrtc-native")]
pub mod whep;

/// Build-capability gating for `[timing].ptp_phc` (DEV-C1 / ADR-M010): a
/// configured PHC device must FAIL a non-`ptp` build at startup with a clear
/// error — never be silently downgraded to the system clock (the DEV-B1
/// fail-fast precedent). Always compiled, so the default build tests the
/// rejection path and a `ptp` build tests the acceptance path.
pub mod timing_gate;
/// The ~1 Hz outbound presentation-epoch publisher (DEV-C1 / ADR-M010): one
/// `WallClockRef` per program on the control WS (`timing.status`, conflated)
/// plus the shared HLS-PDT cell, derived off the hot path from the run's
/// tick-0 anchor and the disciplined wall clock. Never paces the tick loop
/// (invariant #1).
pub mod timing_status;
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

/// The **media-player transport** state machine (ADR-0057 + ADR-0097): the
/// pure, deterministic playout core (load/cue/play/pause/stop/seek, in-place
/// loop, and the vamp-and-exit extension) that drives a pre-declared
/// media-player channel. Feature-independent (no libav/GPU), so it builds and
/// is property-tested in the CI-green default build; the `ffmpeg`-gated ingest
/// executor in [`pipeline`] performs the [`player::PlayerAction`]s it returns.
pub mod player;

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

/// WHIP **ingest** wiring (ADR-T014): the `WhipProvider` implementation over the
/// `multiview-webrtc` native endpoint, the per-source publisher rendezvous
/// registry, and the supervised `drive_webrtc` loop that decodes a publisher's
/// depacketized H.264/Opus into the standard `TileStore`/`AudioStore` pipeline.
/// Behind the off-by-default `webrtc-native` feature (the str0m endpoint +
/// ffmpeg decode); the WHIP source tile rides `NO_SIGNAL` until a publisher
/// arrives and the publisher can never pace/back-pressure the engine
/// (invariants #1/#2/#10).
#[cfg(feature = "webrtc-native")]
pub mod webrtc_ingest;

/// Shared `[webrtc]` config → `multiview_webrtc::config::EndpointConfig` mapping
/// (ADR-0048 §1/§9): the dual-stack UDP port, advertised host candidates, session
/// caps, CORS, and the STUN/TURN ICE servers (incl. the in-driver TURN client's
/// credentials, ADR-0048 §5.1). Used by the WHIP ingest wiring
/// ([`webrtc_ingest`]), the WHEP egress preview wiring ([`whep`]), and the
/// WHEP-serve / `whip_push` output wiring ([`webrtc_outputs`]) so the ICE/TURN
/// mapping is defined once, never duplicated. Behind `webrtc-native`.
#[cfg(feature = "webrtc-native")]
pub mod webrtc_endpoint;

/// WHEP-serve + WHIP-push **output** wiring (ADR-0049): the program is encoded
/// once (invariant #7) and a `webrtc` output WHEP-serves it to N browser viewers
/// while a `whip_push` output publishes it to a remote WHIP ingest, both fed the
/// same coded packets over a bounded drop-oldest egress feed (invariants #1/#10).
/// Behind the off-by-default `webrtc-native` feature (the str0m endpoint + the
/// reqwest WHIP signaller).
#[cfg(feature = "webrtc-native")]
pub mod webrtc_outputs;

/// The **single-socket** WebRTC orchestration (ADR-0048 §4): binds ONE shared
/// dual-stack UDP socket and adopts every WebRTC role onto it — preview WHEP, WHIP
/// ingest, WHEP-serve outputs, and `whip_push` outputs — fixing the box-validation
/// defect B where each role bound `webrtc.udp_port` independently and the 2nd/3rd
/// bind hit `EADDRINUSE` and silently degraded. Behind `webrtc-native`.
#[cfg(feature = "webrtc-native")]
pub mod webrtc_unified;
