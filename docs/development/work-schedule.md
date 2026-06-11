# Multiview — Work Schedule & Fanout Plan

Every remaining incomplete / stubbed / future item, planned to completion with acceptance
criteria, then sequenced for a **dependency‑ and conflict‑aware parallel fanout**. Generated
2026‑06‑05 from an 8‑stream architecture pass (`docs/decisions/` + the live scaffold).

**How to mark off:** flip the box in **Part 2** (`[ ]` → `[x]`) and set the item's `Status:` in
Part 3 when its *Acceptance* criteria are met (tests written + green, invariants re‑asserted).
107 items across 11 work‑streams (originally 48 across 8; the NDI, AES67/Dante and **DEV**
streams plus discovered follow‑on slices were added as they landed): **AUD** audio ·
**OUT** output servers · **NDI** live NDI/NDI|HX · **AES67/DANTE** audio‑over‑IP · **IN** inputs ·
**CTL** control→engine · **PRV** preview/WebRTC · **ENG** engine timing/resilience ·
**GPU** compositor/efficiency/hardware · **SUR** captions/NMOS/web‑codegen ·
**DEV** managed devices & display out.

---

## Part 1 — Execution plan (dependencies → waves → lanes)

### 1a. Logical dependency waves (topological by `deps:` — "cannot *start* until")

A later wave's items depend only on earlier waves. This is the *logical* order; the *parallel*
order is constrained further by file conflicts (1b).

| Wave | Items (can start once the prior wave's deps land) |
|---|---|
| **0** (19) | AUD‑1, CTL‑1, CTL‑3, ENG‑1, ENG‑2, ENG‑4, ENG‑6, GPU‑1, GPU‑2, GPU‑3, GPU‑5, IN‑1, IN‑4, OUT‑1, OUT‑3, PRV‑1, SUR‑1, SUR‑3, SUR‑4 |
| **1** (14) | AUD‑2, CTL‑2, CTL‑4, CTL‑5, GPU‑4, GPU‑6, IN‑2, IN‑5, IN‑6, OUT‑2, OUT‑4, PRV‑2, SUR‑2, SUR‑5 |
| **2** (4) | AUD‑3, CTL‑6, IN‑3, PRV‑3 |
| **3** (6) | AUD‑4, AUD‑5, AUD‑6, IN‑7, PRV‑4, PRV‑5 |
| **4** (1) | AUD‑7 |
| **5** (1) | AUD‑8 |
| **6** (3) | ENG‑3, ENG‑5, SUR‑6 *(deferred polish; ENG‑3 needs ENG‑5's syscall seam)* |

**Critical path (longest logical chain):** `AUD‑1 → AUD‑2 → AUD‑3 → AUD‑4 → AUD‑7 → AUD‑8`
(M→L→L→XL→M→L). The audio‑output spine is the single longest dependency chain and gates `tone`
(AUD‑5) and the audio UI (AUD‑8).

### 1b. File‑conflict map (mutual exclusion — "cannot run *concurrently* in worktrees")

Two items that edit the same file will collide if run as parallel worktree agents. The hotspots:

| Touched by | File | Items |
|---|---|---|
| **9** | `crates/multiview-cli/src/pipeline.rs` | CTL‑6, ENG‑1, ENG‑2, GPU‑1, IN‑3, IN‑5, OUT‑1, OUT‑2, OUT‑4 |
| 4 | `crates/multiview-config/src/schema.rs` | AUD‑7, IN‑4, OUT‑1, OUT‑3 |
| 4 | `crates/multiview-cli/src/control.rs` | CTL‑1, CTL‑2, CTL‑3, CTL‑6 |
| 3 | `crates/multiview-control/src/openapi.rs` | CTL‑4, SUR‑4, SUR‑6 |
| 3 | `crates/multiview-control/src/routes/mod.rs` | CTL‑4, PRV‑2, SUR‑4 |
| 2 | `multiview-output/src/sink.rs` | AUD‑4, OUT‑1 |
| 2 | `multiview-events/src/event.rs` | CTL‑1, CTL‑6 |
| 2 | `cli/src/run.rs` | CTL‑1, GPU‑2 |
| 2 | `control/src/state.rs` | CTL‑3, PRV‑2 |
| 2 | `control/routes/sources.rs` | CTL‑2, SUR‑4 |
| 2 | `cli/.../command.rs` | CTL‑2, CTL‑6 |
| 2 | `multiview-hal/src/load.rs` | ENG‑4, GPU‑5 |
| 2 | `multiview-output/Cargo.toml` | OUT‑2, OUT‑3 |
| 2 | `multiview-preview/src/focus.rs`, `hal/degradation.rs` | PRV‑3, PRV‑4 |

**Headline:** `pipeline.rs` (the data‑plane drive loop) is edited by **9 items from 6 different
streams**. A naive "one worktree per stream" fanout will thrash on it. The data‑plane core
(`pipeline.rs` + `sink.rs` + `run.rs` + `control.rs` + `events/event.rs` + `command.rs` +
`config/schema.rs`) is a tightly‑coupled monolith and must be evolved **serially by one owner**.

### 1c. Recommended fanout — 1 serial integrator + 6–7 parallel lanes

Partition by **file territory** (not by stream), so each lane owns a disjoint set of files and runs
as its own worktree; items *within* a lane are serial in `deps:` order.

- **LANE‑CORE (serial — the long pole, one "data‑plane integrator").** Owns `pipeline.rs`,
  `sink.rs`, `run.rs`, `control.rs`, `events/event.rs`, `command.rs`, `config/schema.rs`.
  Order: **GPU‑1** (encode‑once‑mux‑many — *do first; everything muxes through it*) → **ENG‑1**
  (teardown join) → **OUT‑1/OUT‑3** (output decision + schema) → **CTL‑1 → CTL‑3 → CTL‑2 → CTL‑5**
  (control→engine apply) → **AUD‑2 → AUD‑3 → AUD‑4 → AUD‑5 → AUD‑7** (audio spine) → **IN‑3, IN‑5**
  (NDI/YouTube ingest wiring) → **OUT‑2/OUT‑4** (RTSP/NDI sinks) → **GPU‑2** (software clock) →
  **ENG‑2** (PTS normalizer) → **CTL‑6** (Class‑2). *This lane is the critical path for
  parallelism — keep it staffed.*
- **LANE‑IN (parallel).** `crates/multiview-input/src/{st2110,webrtc,youtube}/` — **IN‑1 → IN‑2 →
  IN‑6**, **IN‑4**, then **IN‑7** (CI). Only IN‑3/IN‑5 (which touch `pipeline.rs`) hand off to CORE.
- **LANE‑PRV (parallel).** `crates/multiview-preview/*` — **PRV‑1 → PRV‑3 → PRV‑4 → PRV‑5**.
  *Coordinate PRV‑2* (adds routes to `control/routes/mod.rs` + `state.rs`) with LANE‑API/CORE.
- **LANE‑ENG (parallel).** `engine/src/{ptp,ha}.rs`, `hal/src/load.rs`, `cli/wallclock.rs` —
  **ENG‑6**, **ENG‑5 → ENG‑3**, **ENG‑4** *(shares `hal/load.rs` with GPU‑5 — merge with LANE‑GPU
  or let one owner hold `load.rs`)*.
- **LANE‑GPU (parallel).** `compositor/*`, `ffmpeg/hwframe.rs`, `hal/select.rs` — **GPU‑3 → GPU‑4**,
  **GPU‑5**, **GPU‑6**. (GPU‑1/GPU‑2 are in CORE.)
- **LANE‑BCAST (parallel).** `control/src/{nmos,is07}.rs` — **SUR‑1**, **SUR‑2**; plus **SUR‑3**
  (captions, `ffmpeg`/`input` — independent).
- **LANE‑API (parallel, but the *cross‑lane coordination hub*).** Owns `control/openapi.rs` +
  `routes/mod.rs` route registration + the codegen — **SUR‑4 → SUR‑5**, **SUR‑6**, and **CTL‑4**
  (ApplyLayout route). Because PRV‑2, CTL‑4, SUR‑4, SUR‑6 all touch `openapi.rs`/`routes/mod.rs`,
  **one owner registers all new routes + OpenAPI annotations** to avoid churn; other lanes file
  their handler bodies and let LANE‑API wire the router.
- **LANE‑WEB (parallel).** `web/src/` — **AUD‑8** (audio matrix UI) after AUD‑7; **SUR‑5** (generated
  client) after SUR‑4.

**Three coordination points to watch in any fanout:** (i) `control/routes/mod.rs` + `openapi.rs`
(CTL‑4 · PRV‑2 · SUR‑4 · SUR‑6) → LANE‑API owns route/spec registration; (ii) `hal/load.rs`
(ENG‑4 · GPU‑5) → one owner; (iii) `control/state.rs` (CTL‑3 · PRV‑2) → agree the `AppState` shape
first.

**So a "complete fanout" = ~7 concurrent worktrees** (CORE + IN + PRV + ENG + GPU + BCAST + API/WEB),
with CORE as the staffed critical path and the three coordination points pre‑agreed. Expect CORE to
dominate wall‑clock; the six parallel lanes finish well before it.

---

## Part 2 — Master checklist


### AUD — Audio pipeline + tone

- [x] **AUD-1** `M` — Logical audio-codec selector + license-aware resolution  ·  _deps: —_
- [x] **AUD-2** `L` — Per-source runtime audio decode thread (peer of video ingest)  ·  _deps: AUD-1_  · _store+loop core: red `4fdd8ca` → green `6042dca` (orchestrated, adversarially reviewed, me-verified): new `multiview-audio::store::AudioStore` — bounded (drop-oldest, never grows), lock-free (`arc_swap` snapshot, wait-free reader), gap-free per-source last-good store on an absolute frame timeline whose `read(frames)` always returns exactly `frames` (silence-filling un-written/evicted spans) — audio sampled-not-pacing (inv #1), cannot back-pressure the engine (inv #10). `audio_decode_loop` (ffmpeg feature): peer of video ingest, opens the !Send libav decoder ON the worker thread, publishes blocks, prompt stop/EOF teardown. `arc-swap` is the SAME 1.9.1 already in-tree (framestore) — no new crate. **cli ingest-supervisor seam SHIPPED (the remaining piece):** new `crates/multiview-cli/src/audio.rs` — `audio_ingest_loop` wraps the pure `audio_decode_loop` in the SAME supervised-reconnect bracket the video `ingest_loop` uses (capped-exponential jittered backoff for live sources; a finite file plays once then rides silence). `Pipeline::build` resolves a per-source `AudioIngestPlan` + empty `AudioStore` for every file/URL source (`audio_ingest_plan_for`); `IngestSupervisor::start` spawns one audio decode thread per plan alongside the video + caption threads (one stop flag, joined/detached the same bounded way), and `start_streaming` routes every store onto the `ProgramBus` at unity gain — so the bus the egress already reads now mixes REAL decoded audio, not silence. Synthetic/NDI/audio-free sources contribute none and ride silence. End-to-end TDD: `program_audio_carries_decoded_source_audio_not_silence` decodes the on-disk `program.ts` audio and asserts non-zero mean-square energy (proves decode→store→bus→mix→encode→mux), with the video still exactly N frames for N ticks (inv #1). clippy default+ffmpeg+overlay, fmt, ws-check all green._
- [x] **AUD-3** `L` — Program-bus mix + per-tick sample budget on the output clock  ·  _deps: AUD-2_  · _Mix CORE shipped + unit-tested: `multiview_audio::cadence::SampleClock` (`7bb0dcb`, red→green) — exact rational per-tick sample budget (NTSC 1601/1602, drift-free, no float/`as`); `multiview_audio::program::ProgramBus` (`c5083a7`, red→green) — composes SampleClock+Mixer+per-source AudioStores into one `tick()`/`tick_to(target_tick)` → `AudioBlock` (gap-free, exact budget, silence for a stalled source, wait-free/no-block → inv #1/#10). **cli hot-loop wiring SHIPPED:** the bake consumer (`consumer_main`, off the output-clock loop) drives `ProgramBus::tick_to(item.tick_index)` per `StreamItem` — RT-8b lip-sync across drop-on-overload gaps — and AAC-encodes the mixed block, fanning it to the SAME sinks (AUD-4). With AUD-2's ingest-supervisor seam now spawning the per-source decode threads + routing their stores onto the bus, the per-tick mix consumes REAL decoded audio. `rt8b_audio_stays_lip_synced_to_the_tick_index_under_video_drops` + `program_audio_carries_decoded_source_audio_not_silence` green._
- [~] **AUD-4** `XL` — Audio encode + dual-stream mux in the output sinks (the core gap)  ·  _deps: AUD-1, AUD-3_  · _OUTPUT-SIDE MACHINERY SHIPPED + me-verified (TDD red→green, clippy default+ffmpeg, fmt, ws-check; 2026-06-06). **Done slices:** (1) `EncodedPacket` `StreamKind{Video,Audio}` tag (`86908eb`) + `EncodeConfig.audio: Option<AudioEncodeConfig>` None-default + `audio_target()` planar-fltp/layout map (`02e364d`); (2) `MuxStream` + `PacketMuxSink::run_av(video, audio)` — register BOTH streams before write_header (ADR-R005 §3.3) + route packets by `StreamKind`, single-container AND per-HLS-segment (only a VIDEO keyframe rotates a segment; audio joins current); `run` stays the video-only delegate so every existing sink/test is unchanged; integration test builds real mpeg2video+aac, interleaves by timeline, asserts file+each segment carry exactly 1 video+1 audio stream, deduping ffprobe's MPEG-TS double-listing (`6b6e0e6`); (3a) `AudioEncoder::send_planar_f32`+`channels()` — builds the libav Audio frame, sample-counter PTS, typed FrameMismatch guards (`19fc1cf`); (3b) `ProgramEncoder` optional AAC encoder + per-channel sample FIFO rebuffering the bus's ~1600-samp/tick blocks into 1024-samp AAC frames + `audio_codec_params()`/`audio_time_base()`/`encode_audio()`; `finish()` flushes audio FIFO remainder+EOF then video (`88ae337`); (3b+) `encode_audio_interleaved` de-interleaves the AudioBlock layout (`4614372`). **REMAINING = slice 4 (cli wiring) + slice 5 (box):** ~70 lines across the PROTECTED streaming seam — keep audio strictly OPT-IN so `audio==None` is provably identical (run_av(video,None)→run delegate; no bus; no audio packets) and every streaming test stays green (GPU-1b lesson). EXACT change map: (a) opt-in — `Pipeline::enable_program_audio()` sets `encode_cfg.audio = Some(AudioEncodeConfig::aac(48_000,2,128_000))`, driven by a new `multiview run --program-audio` flag (cli.rs RunArgs + main.rs run_pipeline) — avoids touching the multiview-config schema; (b) build `Option<ProgramBus>` in `start_streaming` (from `encode_cfg.audio` rate/channels + `cadence`) + thread through `StreamEgress::spawn`→`consumer_main` (ProgramBus is Send); (c) `consumer_main` (pipeline.rs:1779) per `StreamItem`: `if let Some(bus){ let blk=bus.tick(); fan_packets(&sink_txs,&mut live, encoder.encode_audio_interleaved(blk.interleaved(), blk.frame_count())?); }` — finish() already flushes audio; (d) `SinkRunner` (:1491) gains 4th param `audio: Option<(StreamCodecParameters,Rational)>` — update type, `build_sink_runners` (:815), test-seam adapter (:859 `_audio`), spawn call (:1644); `StreamEgress::spawn` snapshots `encoder.audio_codec_params().cloned()`+`audio_time_base()`; (e) `run_one_output`/`run_push_output` (:1884/:1970) build `audio.as_ref().map(|(p,tb)| MuxStream::new(p,*tb))` + call `sink.run_av(...)`. TDD slice 4: cli integration test = bounded streaming run (1 synthetic source + file output, --program-audio) → ffprobe 2 streams (silence audio w/ no sources still proves the path). Slice 5 (box): `multiview run examples/3x3.toml --program-audio --features nvidia` → ffprobe h264_nvenc + aac, audio dur ≈ video. Then flip AUD-3 [x] + AUD-4. **Original design ref:** building blocks ALREADY EXIST — `StreamCodecParameters::from_audio_encoder` (packet.rs:158), `Muxer::add_stream`/`add_stream_from_parameters` multi-stream (mux.rs), `AudioEncoder` (encode.rs:178). GAPS: (1) `EncodedPacket` carries NO stream tag → add a `StreamKind{Video,Audio}` (`from_packet`=Video default, new `from_audio_packet`=Audio) so the fan-out routes; (2) the mux sinks (FileSink/SegmentSink/PushSink `run` + `drive_packets_to_single_muxer`) register a 2nd audio stream from audio `StreamCodecParameters` BEFORE `write_header` (the ADR-R005 §3.3 pinning rule) + route Audio-kind packets to the audio `stream_index` (`Muxer::write_packet` interleaves); (3) `EncodeConfig.audio: Option<AudioEncodeConfig>` (None default = back-compat) + `ProgramEncoder` gains an optional `AudioEncoder` → tagged AAC packets; (4) cli: `ProgramBus` per run (AUD-3 ✓) + spawn AUD-2 `audio_decode_loop` per source + carry program audio in `StreamItem` + consumer encodes it + fans Audio packets. SLICES (keep audio OPT-IN so existing video-only streaming tests stay green — GPU-1b lesson): (1) EncodedPacket tag + AudioEncodeConfig [output, additive, synthetic-packet test]; (2) dual-stream file mux [output, synthetic audio packets → assert 2 registered streams + counts]; (3) consumer AAC encode [cli]; (4) cli ProgramBus+decode+carry; (5) BOX ffprobe 2 streams (h264 video + aac audio, audio dur ≈ video). AUD-3's cli hot-loop wiring folds into slice 4._
- [ ] **AUD-4b** `L` — Multi-rendition / multi-codec program audio (encode-once-mux-many, per inv #7)  ·  _deps: AUD-4_  · _AUD-4 lands ONE audio stream per muxer; AUD-4b makes the encode-once-mux-many rule explicit for audio: encode the program bus once PER DISTINCT audio rendition (codec/sample-rate/bitrate), fan the same audio packets to every transport that shares it, and separate-encode ONLY when the rendition differs (e.g. AAC for HLS/TS + Opus for WebRTC; HLS audio at >1 bitrate). Carry discrete-track/language renditions as TS PIDs / HLS alternate-audio per the ADR-R005 §4.2 capability matrix. Mirror the video `ENCODER_BUILDS`-count test: audio encoder count == number of DISTINCT renditions, not number of sinks._
- [x] **AUD-5** `M` — `bars` synthetic-source 1 kHz tone companion  ·  _deps: AUD-2, AUD-3_  · _SHIPPED (TDD red→green, clippy default+ffmpeg, fmt; me-verified). `multiview-audio::tone::ToneGenerator` — phase-continuous 1 kHz sine at the EBU −18 dBFS line-up level (`LINE_UP_TONE_PEAK_DBFS == VU_REFERENCE_DBFS`), carrying integer absolute-frame phase reduced mod the sample rate (no float drift — inv #3/#6), `f64→f32` only through the clamped helper (no raw `as`); unit tests assert non-silence, the −18 dBFS peak, phase continuity across a block seam, no drift over a long odd-length run, momentary loudness ≈ −18 LUFS (±0.5), and `seek_to_frame` lands on the analytic sine. cli `audio::tone_publish_loop` keeps the bars source's `AudioStore` filled to a bounded lead ahead of the bus read cursor (seeks forward over a never-read `DropOnOverload` span rather than back-filling — bounded burst), off any hot path (inv #1/#10). pipeline.rs touch is MINIMAL + additive (a bars source gets a store in the SAME `audio_stores` map AUD-2's bus routing iterates + a tone plan; `IngestSupervisor` spawns one tone thread/plan, gated on program-audio). Integration test decodes the on-disk `program.ts`: a bars-only run's program audio is non-silent AND its dominant zero-crossing frequency is ~1 kHz (measured 1002 Hz). `solid`/`clock` stay silent._
- [x] **AUD-6** `L` — EBU R128 loudness normalisation on the program bus  ·  _deps: AUD-3_  · _shipped #33 (test `27e130e` → green `4bd19cd`, perf-fix red `50c17e1` → green `bbd971b`): new `multiview-audio::loudnorm` — `LoudnormProcessor` drives a one-pole-smoothed makeup gain off the running short-term `LoudnessMeter` toward `LoudnessTarget` (Broadcast −23 / Streaming −16 / Custom), gated at −70 LUFS (a silenced/lost input is never amplified) with a feedforward oversampled true-peak limiter holding the emitted true-peak ≤ −1.5 dBTP; block shape preserved exactly, discrete tracks bypass byte-identical. Bounded + off the hot path (inv #1/#10): runs on the bake-consumer thread, O(block)/tick with O(1) scalar gain state (`LoudnessMeter::retain_recent` bounds history so memory never grows with run length — the `50c17e1`→`bbd971b` perf fix kept rt8b lip-synced). CLI wiring: `program_loudnorm` applies it between `drive_audio_for_item` and the AAC encode (program-bus only). `crates/multiview-audio/src/loudnorm.rs` + `tests/loudnorm.rs` green: converge ±1 LU, true-peak ceiling never exceeded, gated silence not amplified, discrete byte-identical + monotonic-gain property test._
- [x] **AUD-7** `M` — Audio routing config schema + capability validation  ·  _deps: AUD-3, AUD-4_  · _red `8b30b5e` → green `4357c68` + fix `fefddf1` (schema + audio-crate matrix), then **config-time capability cross-check completed**: `multiview-config::audio` — `AudioRouting`/`AudioRoute` (program-bus membership + per-input `gain_db` + `mute`, discrete `target_track`/`language`, exact-integer `sample_rate_hz`), `AudioChannels`/`OutputAudio`/`OutputAudioMode` (internally tagged, snake_case). The previously-missing piece is now in `multiview-config` itself (NO audio dep): a first-class machine-readable per-output matrix `OutputAudioCapability {delivery: TrackDelivery, discrete_capacity: TrackCapacity, max_channels}` derived by `Output::audio_capability()` from the transport (RTSP/SRT = N simultaneous; HLS/LL-HLS = N select-one; NDI = none/channel-map; legacy RTMP = 1, gated to N by a new `multitrack` endpoint flag). `OutputAudio::validate_against_capability()` is wired into `MultiviewConfig::validate()` and rejects over-capacity selections with the new typed `ConfigError::AudioCapability {output, reason}` (honest "degrade to the mixed bus" message). TDD: `tests/audio_capability.rs` pins the designed-in asymmetry (N tracks pass on SRT/RTSP, rejected on NDI + legacy RTMP, accepted on multitrack-RTMP, select-one on HLS) + the matrix is exported for AUD-8. Reuses the WebUI matrix structure the brief mandates._
- [ ] **AUD-8** `L` — WebUI audio routing matrix + loudness meters  ·  _deps: AUD-6, AUD-7_

### OUT — Output servers (RTSP, NDI)

- [~] **OUT-1** `M` — RTSP egress decision spike + sidecar baseline (MediaMTX) wired as a Push target  ·  _deps: —_  · _red `b9a29d4` → green `6021adc` + review-fix `008d7ec` (orchestrated, adversarially reviewed, me-verified): locks the ADR-0006 decision (in-process gst-rtsp-server = OUT-2) and lands the pure-Rust always-compiled `multiview-output::rtsp` seam — `RtspPublishTarget::new(base, mount)` derives the validated `rtsp://host:port/mount` publish URL (checked formatting, no `as`/indexing/panic; rejects non-rtsp scheme, authority-less base, base-with-path, empty/whitespace mount) + `DEFAULT_BASE` (MediaMTX loopback 8554); under `ffmpeg`, `protocol()` couples to the existing `PushProtocol::Rtsp`→`rtsp` muxer (inv #7, zero new sink code). Review-fix closed two minor defects (broken default-`cargo doc` intra-doc links + an accepted authority-less `rtsp://`). 12 always-CI tests + 1 ffmpeg-gated; the live network push is `#[ignore]`-with-reason (no MediaMTX peer in CI). **Remaining → OUT-2:** in-process RTSP server; + the cli `Output::RtspServer` wiring, a sidecar-gated `push_rtsp_ffprobe` live test, and a `deploy/` MediaMTX manifest._
- [~] **OUT-2** `XL` — In-process RTSP server via `gst-rtsp-server`, fed pre-encoded NALs (`PacketSink`)  ·  _deps: OUT-1_  · _red `a07fdfb` → green `3948c5a` (orchestrated, REVIEWER-APPROVED, me-verified): the pure-Rust typed seam is ALWAYS compiled + CI-tested — `BoundedPacketQueue` (drop-oldest, push never blocks/grows — inv #1/#10), `RtspServerSink: fanout::PacketSink` (non-blocking, shares the `Arc<EncodedPacket>` — encode-once inv #7), `RtspMount` (validated `rtsp://host:port/mount`), `RtspCodec`+`units_to_nanos`. The actual gst-rtsp-server serving (GLib loop, `set_shared` factory, appsrc need-data pump) is behind a NEW off-by-default `rtsp-server` feature (gstreamer/glib bindings MIT/Apache, optional; LGPL C libs dynamically linked, never vendored; default build pulls ZERO gst). 18 CI tests + 1 `#[ignore]` live playout. **Remaining → OUT-2b:** verify the gst serving path on a gst-equipped runner (the test box can `apt install` gstreamer) — live ffprobe pull, second-client `set_shared` fan-out, slow-client isolation; + the cli `Output::RtspServer` wiring._
- [ ] **OUT-2b** `M` — RTSP server: validate the gst serving path + cli wiring  ·  _deps: OUT-2_  · _on a gstreamer-equipped runner (apt install gstreamer1.0 + gst-rtsp-server on the test box) verify the `rtsp-server` feature compiles+links + serves: ffprobe pull of the served `rtsp://`, second-client fan-out, slow-client isolation; wire `Output::RtspServer` into pipeline.rs build_outputs/run_one_output._
- [x] **OUT-3** `L` — NDI dynamic-load backend (`NDIlib_v6_load`) + feature/license scaffolding  ·  _deps: —_  · _`407056a`: NEW `multiview-ndi-sys` FFI crate (deny+SAFETY dlopen/dlsym/NDIlib_v6_load via libloading ISC, opaque NonNull table — no SDK vendored, no build-time link) + `multiview-output::ndi` (loader path-search, runtime license gate, attribution constants, `NdiOutput` sink seam over a fake API). multiview-output stays forbid(unsafe). Live send over the real SDK table + cli `RunnableOutput::Ndi`/NV12→UYVY/`[system.ndi]` license config = OUT-4 + cli glue (gated)_
- [~] **OUT-4** `L` — NDI output Sender wired as a Sink (host-memory copy from canvas)  ·  _deps: OUT-3_  · _red `1af4a76` → green `405d942` + review-fix `8889273` (orchestrated, reviewed, me-verified): the pure-Rust `multiview-output::ndi::convert` seam — `Nv12Canvas` view (geometry/plane-length validated), `nv12_to_uyvy` (4:2:0→UYVY 4:2:2, vertical chroma replication, checked-indexing only) + `NdiOutput::send_canvas` (tick-derived timecode inv #3, typed `InvalidFrame`/`Closed` refusals, never panics). 8 CI tests under the `ndi` feature (I added the 4x4 chroma-row-stride case the reviewer flagged missing). Default build NDI-free/LGPL-clean. **Remaining → OUT-4b:** the cli runnable egress (`RunnableOutput::Ndi` + `run_one_output` arm + `[system.ndi]` accept-license config + canvas fan-out) and the hardware-gated `ndi_output_roundtrip` (in-process Finder/Receiver) — the agent was scoped to multiview-output only, so the CLI half is genuinely deferred._
- [ ] **OUT-4b** `M` — NDI output: cli runnable egress + roundtrip test  ·  _deps: OUT-4, NDI-L1_  · _wire `RunnableOutput::Ndi` into `pipeline.rs` build_outputs/run_one_output (canvas → `NdiOutput::send_canvas` off the hot path), add `[system.ndi]` accept-license config field, and the named `tests/ndi_output_roundtrip.rs` (feature `ndi`, in-process Finder/Receiver, content-aware ramp survival, #10 re-assert). Hardware-gated where a real runtime is needed._

### NDI — FULL live implementation (user directive 2026-06-06: NDI + NDI|HX fully implemented & tested, license-gated)

> **As-built (mapped 2026-06-06):** the entire SAFE SEAM exists on BOTH sides + is unit-tested with fakes, and the **license gate is already enforced by construction** (`multiview-output::ndi::license::NdiLicense`, `multiview-input::ndi::license::NdiLicense` — `NdiOutput::new`/ingest require an `NdiLicense` by value; `from_setting(false,_)` = `NotAccepted`, blank audit = `IncompleteAcceptance`). `multiview-ndi-sys` runtime-loads `NDIlib_v6_load` (dlopen, opaque table, never vendored). Conversions (NV12↔UYVY/BGRA), `NdiApi`/`NdiReceiver` traits, `FakeNdiApi`/`FakeNdiReceiver`, attribution constants, loaders, capability matrix — ALL present + tested. **The gap is the LIVE binding + advanced features.** Constraints (ADR-0008 + docs/io/ndi.md): never vendor the SDK (runtime-load only); declare the C ABI structs ourselves from the documented ABI (that is normal FFI, not copying the proprietary SDK); mandatory attribution; host-memory copy at the NDI edge; FrameSync per ingest source; NDI YUV = limited-range by resolution matrix.

- [~] **NDI-L0** `S` — devcontainer NDI SDK provisioning (gated)  ·  _deps: —_  · _BLOCKS NDI-L1: the live binding needs the authoritative NDIlib v6 HEADER to declare the ABI soundly (guessing the function-table offsets = UB — verify-don't-assume + FFI-safety), and the loopback test needs the libndi RUNTIME; both come from the NewTek/Vizrt NDI SDK (NDI is a Vizrt product — NOT Audinate, which makes Dante) whose download = accepting its licence. **Added (2026-06-06) an opt-in, never-vendored devcontainer stage** (.devcontainer/Dockerfile): `--build-arg NDI_SDK_ACCEPT_LICENSE=1 --build-arg NDI_SDK_URL=<operator's SDK tarball>` installs headers→/opt/ndi/include + libndi→/usr/local/lib (+ sets `NDI_RUNTIME_DIR_V6`, which `multiview-ndi-sys` already honors); default OFF = open/LGPL-clean. **REMAINING: operator supplies the SDK URL + rebuilds the image** (Vizrt gates the download behind a form → no public URL to hardcode; the .sh-installer variant needs EULA flags). Until then NDI-L1 is correctly blocked — the open AES67 path (AES67-1..5, ADR-T010) is the unblocked Dante-audio route._
- [ ] **NDI-L1** `XL` (KEYSTONE) — Live SDK function-table binding  ·  _deps: NDI-L0_  · _DESIGN: ADR-0028 (resolve-once flat NdiV6 table; typestate async-send pinning + free-on-Drop recv; safe NdiSender/NdiFinder/NdiReceiver boundary; regenerate-not-commit bindgen). Bindgen foundation done + gpu-test-box-validated (7a6141d)._  _In `multiview-ndi-sys` (the ONLY `allow(unsafe_code)` leaf), declare the `NDIlib_*` C ABI structs (`video_frame_v2_t`, `audio_frame_v3_t`, `send_create_t`, `recv_create_v3_t`, `source_t`, `find_create_t`, `tally_t`) from the DOCUMENTED ABI (not vendored), resolve the function pointers off the loaded v6 table (`send_create/send_send_video_v2/send_send_audio_v3/send_get_tally/send_destroy`, `recv_create_v3/recv_capture_v3/recv_free_*/recv_destroy`, `find_create_v2/find_get_current_sources/find_destroy`), each `// SAFETY:`-justified. Then implement `SdkNdiApi: multiview_output::ndi::NdiApi` and `SdkNdiReceiver: multiview_input::ndi::NdiReceiver` over the table (the higher-level crates stay `forbid(unsafe)`). This is what makes everything below REAL — today only the fakes exist. TEST: gated loopback (send→find→recv on one host) on a runtime-equipped runner._
- [ ] **NDI-L2** `M` — Egress live wiring (folds OUT-4b)  ·  _deps: NDI-L1_  · _`RunnableOutput::Ndi` + `run_one_output` arm feeding `NdiOutput::<SdkNdiApi>::send_canvas` off the hot path; `[system.ndi] accept_license` config → `NdiLicense::from_setting`; loopback roundtrip ffprobe/receiver test._
- [ ] **NDI-L3** `M` — Ingest live wiring (folds IN-3b)  ·  _deps: NDI-L1_  · _construct `SdkNdiReceiver` into the ingest supervisor for `SourceKind::Ndi` (per-source FrameSync, host→GPU copy, LIVE→STALE→RECONNECTING state machine); wire `NdiLicense::from_setting` into `ingest_plan_for` (unaccepted ⇒ tile degrades, never silent); loopback NV12 roundtrip test._
- [ ] **NDI-L4** `L` — Discovery (`NDIlib_find`)  ·  _deps: NDI-L1_  · _enumerate live sources; `GET /api/v1/discovery/ndi` control endpoint + WebUI source picker; optional `{kind:'ndi', discover:true}` auto-select. Bounded, off the engine path (#10)._
- [ ] **NDI-L5** `M` — Audio over NDI (send + recv)  ·  _deps: NDI-L1, AUD-4_  · _`send_send_audio_v3` for the program bus (channel-map, the capability matrix already validates this) + `recv` audio → program clock rebase. Planar-float, ≤2ch AAC cap per the matrix._
- [ ] **NDI-L6** `M` — Tally + metadata  ·  _deps: NDI-L1_  · _`send_get_tally` → engine tally arbiter; extend `ReceivedFrame` (already `#[non_exhaustive]`) with metadata/timecode variants._
- [ ] **NDI-RECV-OPEN** `L` — OPEN, default-build Full-NDI **receive** (no proprietary SDK)  ·  _deps: —_  · _Research (docs/research/ndi-integration.md) found a licence-free RECEIVE path: VideoLAN **libndi** (C, LGPLv2.1, discovers + receives Full NDI) + FFmpeg's reverse-engineered **SpeedHQ decoder** (libavcodec/speedhq.c, since 2017) — so NDI INGEST (Full NDI, video+audio) can ship in the DEFAULT/LGPL-clean build with NO NewTek/Vizrt SDK. No send, no HX (those stay SDK-gated, NDI-L*/NDI-HX). Evaluate link-libndi vs port-the-receive-path; feed UYVY→NV12 into the existing `NdiReceiver` seam. The best-of-both: everyone gets NDI ingest; licensed operators get send + HX._
- [ ] **NDI-HX** `XL` — NDI|HX (Advanced SDK, compressed H.264/HEVC)  ·  _deps: NDI-L1_  · _RESEARCH-CONFIRMED (ndi-integration.md): HX **decode is FREE via the standard SDK** (a normal receiver gets uncompressed frames from HX/HX2/HX3 sources — covered by NDI-L3). HX **encode/send** = the NDI **Advanced SDK** (separate licence + vendor ID): `NDIlib_send_create_v2` + submit `NDIlib_compressed_packet` carrying OUR H.264 (HX) / HEVC (HX2/3) bitstream (we already produce these via NVENC/x264-5). Confirm the trial covers Advanced._ _SEPARATE off-by-default `ndi-advanced` feature + its OWN license attestation (Advanced SDK is separately licensed; H.264/HEVC royalties are the operator's). HX-RECEIVE comes largely free via the standard SDK transparently decoding HX sources once NDI-L3 lands — so prioritise documenting/validating HX-recv there. HX-SEND needs the Advanced-SDK entry point + compressed-frame send path. Both inert until the operator attests the Advanced license (mirror the NdiLicense gate)._

### AES67 / ST 2110-30 audio-over-IP — the OPEN Dante interop path (research done → [ADR-T010](../decisions/ADR-T010.md), [ADR-0033](../decisions/ADR-0033.md), brief [aes67-delivery.md](../research/aes67-delivery.md))

> **Decision (ADR-T010):** Audio-over-IP and Dante interop are delivered via the OPEN **AES67 / SMPTE ST 2110-30** standard (Audinate's own licence-free bridge) — **native Dante is NOT supported** (its SDK is, per Audinate's response to us on file, intended for products under commercial licensing agreements and not available for open-source integration; a reverse-engineered native path is not shippable for an open product). Multiview already owns the load-bearing part: the ST 2110-30/AES67 L16/L24 PCM depacketizer + RTP parser (`multiview-input/src/st2110/v30.rs`, property-tested) + the PTP servo. Dante-AES67 interop budget: L24/48k/1ms ptime/≤8ch/239.x16/PTPv2 domain 0/SAP-SDP discovery. TEST: gated AES67 RTP loopback (wire contract) + on the box `linuxptp` (ptp4l) GM + ffmpeg AES67 counterpart; **DVS is NOT AES67-capable so it can't be the interop target** (real Dante-over-AES67 needs AES67 hardware + SAP).

- [ ] **AES67-1** `L` — Audio `FrameProducer` over the `v30` depacketizer → `AudioStore` (Dante/AES67 INGEST; single-path `RtpReceiver`, sampled-not-pacing #1)  ·  _deps: IN-2, AUD-2_
- [x] **AES67-2** `M` — AES67-audio SDP parse + generate (RFC 4566/8866: `m=audio RTP/AVP`, `a=rtpmap L24/48000/ch`) — fills channels/depth/ptime the depacketizer needs  ·  _deps: AES67-1_  · _shipped #33 (test `1ac7e32` → green `371227b`): `multiview-input/src/st2110/sdp.rs` — RFC 4566/8866 `m=audio` + RFC 7273 ts-refclk/mediaclk codec parses/emits rtpmap (L16/L24, 48k/96k, channels), ptime, ptp/localmac ts-refclk and `mediaclk:direct` into one `AudioSdpSession`. Packet time carried as integer thousandths-of-a-ms (Class B 0.125 ms round-trips exactly) — never a float (inv #3); IPv6-first. 21 SDP tests green in `tests/aes67_sdp.rs`. (Note: AES67-2's listed dep AES67-1 ingest is NOT yet shipped — the SDP codec itself is complete and offline-verifiable.)_
- [ ] **AES67-3** `M` — SAP announce + listen (RFC 2974, UDP 9875, 224.2.127.254) for discovery both directions; DDM-proxy note for legacy Dante  ·  _deps: AES67-2_
- [~] **AES67-4** `L` — AES67/ST 2110-30 RTP **transmit** from the program bus (L24/48k/1ms, 239.x/16); PTP-referenced media clock; bounded/off-hot-path (#1/#10)  ·  _deps: AES67-2, AUD-3, AUD-4_  · _offline packetizer slice shipped #33 (test `1ac7e32` → green `371227b`): `Aes67Packetizer` (`multiview-input/src/st2110/packetize.rs`) — the exact inverse of `V30Payload::parse`, f32 PCM → big-endian L16/L24 RTP payload bytes (L24 scales by 2^23−1, not 2^23, so full-scale positive does not wrap to most-negative); shares one `Aes3Format`/`SampleDepth` wire model with the depacketizer. 10 round-trip tests green (`tests/aes67_packetize.rs`). **Remaining:** the live RTP **transmit from the program bus** + PTP-referenced media clock + bounded off-hot-path send loop (the box-gated follow-on) is NOT wired — keep open._
- [~] **AES67-5** `M` — PTP profile/domain config (AES67 media profile / ST 2059, domain 0) on the existing servo; config `SourceKind`/`Output`; gated loopback interop test + chaos-gate #10; box test = ptp4l GM + ffmpeg AES67 counterpart  ·  _deps: AES67-3, AES67-4, `ptp`_  · _config-schema slice shipped #33 (test `1ac7e32` → green `371227b`, + clippy fix `cbeaf10`, AUD-7 cross-check `5110386`): `multiview-config` gains `SourceKind::Aes67` (static SDP + optional session_id/multicast/link_offset_ms/ptp_domain) and `Output::Aes67` (first output with no encode stage; always-None gpu_pin keeps `Output::gpu_pin` a uniform match; label/multicast/depth/ptime_ms/ptp_domain/audio with L24 + 1 ms Class-A defaults), internally tagged, IPv6 multicast literals; every exhaustive Output/SourceKind match updated; 9 schema tests green (`tests/aes67_schema.rs`). **Remaining:** the PTP profile/domain wiring onto the live servo + the gated ptp4l/ffmpeg loopback interop + chaos-gate #10 test (box-gated follow-on) is NOT done — keep open._

> **Native Dante: NOT a planned feature** ([ADR-T010](../decisions/ADR-T010.md)). The earlier `DANTE-1..5` native-SDK tasks are removed — native Dante depends on an Audinate SDK which, per Audinate's response to us on file, is intended for products under commercial licensing agreements and not available for open-source integration, and AES67 / ST 2110-30 (above) already reaches Dante facilities through Dante's AES67 mode. There is no `multiview-dante-sys` leaf and no native-Dante feature in the build.

### IN — Inputs (NDI · ST 2110 · WebRTC · YouTube)

- [x] **IN-1** `M` — ST 2110 receive: frame assembler over the depacketizers  ·  _deps: —_  · _pure SRD/line reassembler (marker/seq/timestamp) tested; pgroup→NV12 unpack + `ProducedFrame` adaptation belong with IN-2_
- [x] **IN-2** `L` — ST 2110 receive: wire `RtpReceiver`/`DualPathReceiver` into a `FrameProducer` + PTP timing  ·  _deps: IN-1_  · _`549b971`: `St2110Producer: FrameProducer` over the IN-1 assembler + 90 kHz→ns rebase (`WrapBits::Rtp32`, float-free, monotonic) + 2022-7 `DualPathPacketSource` dedup + bounded async→sync bridge; injected-packet tested, live NIC/PTP gated. cli wiring = IN-3_
- [~] **IN-3** `XL` — NDI ingest: runtime-loaded SDK → `FrameProducer` + CLI wiring  ·  _deps: IN-2_  · _red `0496181` → green `db8de12` + my fix `40c0ba0` (orchestrated, reviewed, me-verified): `multiview-input::ndi` — checked-indexing UYVY→NV12 (4:2:2→4:2:0 chroma avg) + BGRA→NV12 (BT.709 fixed-point), an `NdiReceiver` sample seam + injected `FakeNdiReceiver`, `NdiProducer: FrameProducer` (sampled/non-blocking, NDI-100ns→ns, `WrapBits::None`) driving the IN-2 `IngestPump`; cli `SourceKind::Ndi` planning behind the `ndi` feature (now correctly implies `ffmpeg` since the run wiring is in the ffmpeg-gated pipeline). `forbid(unsafe_code)` (FFI in `multiview-ndi-sys`). Off-by-default/LGPL-clean. **Remaining → IN-3b:** wire the `NdiLicense` accept-gate into `ingest_plan_for` (defined but unwired), the live NDI receive + roundtrip (needs the runtime + a sender — `#[ignore]`), tidy the near-tautological probe test + stale CI comment (IN-7)._
- [ ] **IN-3b** `M` — NDI ingest: license-accept gate in ingest + live roundtrip  ·  _deps: IN-3_  · _wire `NdiLicense::from_setting` into `ingest_plan_for` so an unaccepted NDI source degrades its tile (not silently); add the runtime-gated live receive + NV12 roundtrip test (NDI runtime + a live sender); strengthen the probe test._
- [x] **IN-4** `M` — YouTube live: pure resolver core over `yt-dlp -J`  ·  _deps: —_
- [~] **IN-5** `L` — YouTube live: wire to HLS ingest + re-resolution loop  ·  _deps: IN-4_  · _red `a231b46` → green `7a0ba8d` + review-fix `67360e7` (orchestrated, reviewed, me-verified): new `multiview-input::youtube::reresolve` — pure `ReresolveSchedule` (refresh `lead` before `expire`, `ttl_guard` fallback + clamp, 403-burst trigger) + injectable `Resolver`/`UnixClock` + the supervised `run_reresolve_loop` (make-before-break, backoff, inv #1/#10), all unit-proven with fakes/paused-time. cli `youtube` feature: `SourceKind::Youtube` resolves a fresh `*.googlevideo.com` master on every (re)connect via yt-dlp, so the reconnect bracket survives the ~6 h expiry. I fixed the blocking no-aspirational-comments finding (the module doc claimed before-expiry refresh the wired path doesn't do — now an honest "Wiring status" section) + two inaccurate test comments. Default LGPL-clean (youtube off). **Remaining → IN-5b:** spawn `run_reresolve_loop` PROACTIVELY into the cli ingest supervisor (the async-loop↔sync-decode-thread bridge + swappable-URL slot) so it refreshes BEFORE expiry (make-before-break) instead of degrading the tile at the boundary._
- [ ] **IN-5b** `M` — YouTube: wire the proactive lead-time re-resolution loop into cli ingest  ·  _deps: IN-5_  · _spawn `run_reresolve_loop` as a supervised sibling of the youtube decode thread with a shared swappable-URL slot (async↔sync bridge), so a long run refreshes the manifest `lead` seconds before `expire` (make-before-break) rather than the current re-resolve-on-reconnect (tile briefly degrades at the boundary). The loop + policy already exist + are unit-tested (IN-5)._
- [~] **IN-6** `XL` — WebRTC ingest: ICE/DTLS/SRTP transport behind an application-layer media engine  ·  _deps: IN-1_  · _`831e3af`: testable core landed behind `webrtc` — session lifecycle + `MediaEngine` seam + RFC-6184 H264 depacketizer (keyframe-gated, bounded) + `WebRtcProducer: FrameProducer`, fake-driven tests; live ICE/DTLS/SRTP engine (str0m) + cli wiring + Opus/VP8 = **IN-6b**_
- [x] **IN-7** `S` — CI strategy: feature-gated compile + integration gating for the wired transports  ·  _deps: IN-2, IN-3, IN-6_  · _`871db84`: `.github/workflows/ci.yml` `feature-clippy` matrix (9 legs: st2110/webrtc/youtube/ptp/cluster/ntp/is07-mqtt/nmos/i915-pmu, all -D) + `asyncapi-validate` job (SUR-6b CI tail); commands verified locally. NDI(IN-3) excluded (no feature yet — needs an SDK-fetch Docker lane)_

### CTL — Control plane → engine

- [~] **CTL-1** `L` — Drain-apply every accepted command + emit outcome events  ·  _deps: —_  · _red `73118df` → green `c3c8e09` + gap-fix `99d3ace` (orchestrated, adversarially reviewed, me-verified + gap-closed): `command_drain` now applies EVERY accepted command at the frame boundary + emits an outcome (no more 202-then-no-op). Threads a non-blocking `Arc<EnginePublisher>` in; Start/Stop→`OutputStatus{Running|Idle}` (added `OutputRunState::Idle`); ApplyLayout re-applies the working layout; Arm/Take/CancelSalvo apply the salvo's source-recalls + emit `Salvo{Armed,Taken,Cancelled}`; SetTallyOverride→`TallyState` echo; SwapSource keeps its swap. My gap-fix added the Acceptance-mandated engine-level soak (`run_for_with_control` + `control_command_flood_never_falters_the_output_clock`: a saturated bus flooded across 120 ticks → frames==ticks, !faltered — inv #1+#10) + renamed a `state` shadow. 8 control/software_run tests green. **[~] honest scope (downstream subsystems not built):** ApplyLayout can only re-apply the single working layout (no named-layout LIBRARY → CTL-4/CTL-2), salvo applies sources-only (no layout-preset/tally/umd — no arbiter), tally is an echo (no arbiter). No event variant invented for SwapSource._
- [ ] **CTL-2** `L` — Apply Source/Output/Overlay CRUD to the running engine via the command bus  ·  _deps: CTL-1, CTL-3_
- [~] **CTL-3** `M` — Mirror `multiview run` config into the resource store at startup + on change  ·  _deps: —_  · _red `51a60a2` → green `ec85414` (orchestrated, REVIEWER-APPROVED, me-verified): `multiview_control::seed_resources(&MultiviewConfig)` + `AppState::with_seeded_resources` mirror one resource per config source/output/overlay (body = `serde_json::to_value`, round-trips back to the typed value) + the working layout into fresh in-memory stores; `bind_and_serve` seeds once at bind time (off the engine hot loop, inv #10) and both `main.rs` call sites pass `&config`. 5 seed unit tests (counts/ids/body round-trip/empty-config). No new deps. **[~] honest scope:** startup-seed done; the "on change" mutation-sync half is owned by CTL-2/the CTL-1 apply path (not this item). Reviewer minor: the Acceptance named an HTTP-level `GET /api/v1/sources` integration test — delivered as 5 lib-level seed tests + the wiring verified by the reviewer (stores install into the exact `AppState` fields the routes read); the authed HTTP assertion is a belt-and-braces follow-up._
- [x] **CTL-4** `S` — `ApplyLayout` HTTP route  ·  _deps: CTL-1_  · _red `266ebc2` → green `a271dfc` (orchestrated, REVIEWER-APPROVED clean, me-verified): POST /api/v1/commands/apply-layout mirroring cmd_swap — `require(Action::Write)` → `authorize_object` → `submit_accepted(|op| Command::ApplyLayout{op,layout})` (202+op-id, idempotency + shed-503 free) → audit; registered in router + `#[utoipa::path]` + `ApplyLayoutRequest` schema so /docs + /api/v1/openapi.json surface it. 4 route tests (202+op, 403 viewer, 503 bus-full, present-in-openapi). inv #10 (try_submit only). No new deps._
- [x] **CTL-5** `M` — Salvo/Start/Stop outcome events on the realtime stream (corr-correlated)  ·  _deps: CTL-1_  · _red `7dd8224` → green `0b4b1aa` + my doc-fix `40c0ba0` (orchestrated, REVIEWER-APPROVED after fix, me-verified): a control-plane-only `CorrRegistry` carries the accepted command's `OperationId` onto its outcome `Envelope.corr` (ADR-W008) WITHOUT adding an op id to the `Event` enum or touching the engine — `submit_accepted` records `(CorrKey, op)` at 202 time; the realtime projection resolves+memoizes it per engine seq so every fanned-out subscriber stamps the same corr (a re-emitted outcome carries none). Covers start/stop + named salvo arm/take/cancel; SwapSource/ApplyLayout (no dedicated event) + armed-salvo `salvo:None` take are uncorrelated by design (corr:None, never wrong). 6 corr tests + a lagged-skip inv-#10 test. The blocking finding (a nonexistent `take` doc link) is fixed._
- [ ] **CTL-6** `XL` — Class-2 parallel-output (make-before-break) migration  ·  _deps: CTL-1, CTL-2, CTL-4, CTL-5_

### PRV — Preview & WebRTC transport

- [~] **PRV-1** `XL` — Native ICE/DTLS/SRTP transport behind a `WhepTransport` seam (str0m, in-process default)  ·  _deps: —_  · _SEAM landed `befefb2` (trait + SDP offer/answer glue + session lifecycle + bounded drop-oldest feed, fake-transport tested); native str0m ICE/DTLS/SRTP impl is **PRV-1b** below_
- [x] **PRV-2** `L` — Wire WHEP focus routes into `multiview-control` (POST/DELETE per scope) with token-gated Focus + transport seam  ·  _deps: PRV-1_  · _`daedd1d`: POST/DELETE `/preview/{program,inputs/{id},outputs/{id}}/whep` over a codec-free `WhepProvider` seam (offer→201+sdp answer; 401/403 token-gated; 404/415/503-fallback RFC9457); openapi-documented; 7 route tests. Binary adapter bridging to preview's `WhepTransport` = PRV-1b glue_
- [x] **PRV-3** `M` — Concurrent-focus session caps + isolation enforcement (the `FocusGate`)  ·  _deps: PRV-2_  · _`a67dc24`: `FocusGate` (global + per-scope caps, fail-closed, `FocusLease` Drop frees) + `GatedWhep` decorator admitting before delegate → existing `503 fallback` shed shape; 5+4 tests. HAL cost-budget hook = PRV-4_
- [x] **PRV-4** `M` — Make preview the topmost (cheapest-to-shed) degradation rung  ·  _deps: PRV-3_  · _`a6023db`: 5 preview rungs prepended ABOVE every tile/program rung in `multiview-hal::degradation` (13-rung ladder, `affects_preview()`/`first_program_level()`) + `FocusGate::suspend()/resume()` hook refusing new focus while degraded (503-fallback); 14 hal + preview tests prove preview is fully shed before any program lever. The cli degradation-loop glue that observes `Hysteresis` and calls `suspend()/resume()` (+ tracing) is **PRV-4b**_
- [~] **PRV-5** `L` — Sub-second WebRTC OUTPUT (program) focus: program-canvas tap → preview encode → WHEP  ·  _deps: PRV-1, PRV-2, PRV-3_  · _`ec47b48`: preview-side SEAM landed+tested — `ProgramTap`(lazy/last-leave-stop) → `PreviewEncoder` seam → bounded drop-oldest feed → `ProgramFocusSession` (FocusGate cap + tap lease, Drop frees both); inv-#1/#10 proven (1000 publishes never block). cli tap-wiring + output `PacketSink` registration + live str0m = **PRV-5b**_
  - [ ] **PRV-5b** `M` — Program-output WHEP wiring: register a preview `PacketSink` on `multiview-output::fanout` (route()+1, encode-once preserved) + cli program-canvas tap → `ProgramFocusSource` + `routes/preview.rs` `preview/source` real-vs-approx label  ·  _deps: PRV-5_  ·  _→ in scope of the **webrtc-full push** (ADR-0048/T014/0049/P006/W020)_

### ENG — Engine timing & resilience

- [x] **ENG-1** `M` — Bounded teardown join for a wedged sink (task #50)  ·  _deps: —_  · _`5ca8eab`: `StreamEgress::join` waits ≤`SINK_WEDGE_GRACE` (2s) per sink via `is_finished()`, then reports+detaches a wedged sink (never a `join()` that can't return); consumer fan-out uses a bounded `send_bounded` (try_send poll) so a never-draining sink can't stall it either. Watchdog-thread TDD test (RED hung at 8s → GREEN ~2s)_
- [~] **ENG-2** `XL` — Input PTS normalizer + pacer reroute (ADR-0021 points 1-3)  ·  _deps: —_  · _ENG-2 red `dab5a9b` → green `8c581a7`: the CLI ingest now routes each decoded frame's **raw** source-tick PTS (new `DecodedVideoFrame.raw_pts`) through the per-input `PtsNormalizer` via a `timeline_pts` seam in `open_and_stream` — 33-bit delta-unwrap, genpts fallback, discontinuity re-anchor, strict-monotonic guard onto one ns timeline (inv #3) before pace/publish. Deterministic cli tests: monotonic-across-33-bit-wrap + genpts-advances. The normalizer's full adversarial matrix is unit-proven in `multiview-input/tests/normalize.rs`. **Remaining (ENG-2b):** did NOT retire `PtsWallClock` (it already meets inv #4 and now consumes the normalized timeline) nor wire `multiview-input::Pacer`/`ReorderBuffer` (the libav decoder already presents frames in PTS order, so packet-reorder is a raw-RTP/ST-2110 concern, not this demux path); content-aware wrap-boundary end-to-end is the GPU/soak tier_
- [x] **ENG-3** `M` — NTP/PTP lock auto-detect for the wall-clock badge (task #37)  ·  _deps: ENG-5_  · _`d87b69d` (engine `sysref` classifier + `multiview-ntpsys` FFI crate) + ENG-3b `96ec96c` (cli wiring): the wall-clock badge now reports the MEASURED kernel discipline under the cli `ntp` feature. Holdover-dwell hysteresis is a tracked refinement (ENG-3c, optional)_
- [x] **ENG-4** `L` — Linux i915/amdgpu GPU load probe  ·  _deps: —_  · _sysfs busy%+VRAM probe (`SysfsLoadProbe`, PCI-bus keyed) + pure parsers tested; per-engine enc/dec via `/proc/pid/fdinfo` walk + i915 PMU (needs unsafe) are follow-up slices_
- [x] **ENG-5** `L` — PTP / ST 2059 PHC NIC binding (`ptp` feature)  ·  _deps: ENG-3_  · _`9cb742b`: lock-state machine + offset servo (pure, tested) + Linux `/dev/ptpN` read via `rustix` (no unsafe), live test gated_
- [x] **ENG-6** `L` — HA cluster peer transport (`cluster` feature)  ·  _deps: —_  · _`UdpClusterTransport` + failover/replication over loopback-tested; true multi-host partition is hardware-tier (gated)_

### GPU — Compositor, efficiency & hardware

- [x] **GPU-1** `L` — Hoist the single encoder into the bake consumer; fan packets to mux-only sinks  ·  _deps: —_  · _DONE via `357d52b` (`ProgramEncoder`) + `a1af76c` (GPU-1b wiring): the consumer encodes ONCE, fans `EncodedPacket`s to N `PacketMuxSink`s. Verified end-to-end — one run encoded 50 frames once and fanned them to program.ts (50) + HLS (seg0 25 + seg1 25), all decodable mpeg2video; streaming inv-#1/#10 suite still 4/4_
- [x] **GPU-2** `M` — Converge the SOFTWARE engine onto `synth::generator_loop` so a clock source animates  ·  _deps: —_  · _red `70a0d1b` → green `fbf724e` + my fix `40c0ba0` (orchestrated, REVIEWER-APPROVED, me-verified): `SoftwareEngine::build` classifies each source via `SyntheticKind::from_source_kind`; static (bars/solid) + decoded keep prime-once, an animated `clock` is recorded for a `GeneratorSupervisor` thread (one `synth::generator_loop` per animated source publishing into the lock-free `TileStore` the engine only samples — inv #1/#10) and is NOT primed (no double-publish), only under `overlay`; without `overlay` it falls back to the honest primed placeholder. Stop bridged across `run_for`/`run_for_realtime`/`run_until_stopped*`. Content-aware test asserts the clock tile's y-plane changes across a real second; the static bars/solid + frames==ticks tests stay green. (Fixed the pre-existing `--features overlay` wallclock clippy.)_
- [x] **GPU-3** `S` — GPU `describe_*` metadata trait methods: wire or remove  ·  _deps: —_
- [~] **GPU-4** `L` — Overlay IMAGE-primitive GPU texture upload (the wgpu shader branch)  ·  _deps: GPU-3_  · _`8fd5d01`: WGSL `KIND_IMAGE` premultiplied blit + upload-once content-keyed texture cache + packing/bind-entry (CPU seams tested, naga-validated); runtime dispatch + SSIM/PSNR parity need a GPU runner → **GPU-4b** below_
- [~] **GPU-5** `XL` — Multi-GPU PLACEMENT decision engine: closed-loop controller + config + telemetry  ·  _deps: —_  · _PARTIAL: HAL deliberate-split decision (`split.rs`: `plan_split`/`CutPoint`/`CrossGpuCopy`) landed `c995341`; the engine controller (sustained-overload SHED-vs-MIGRATE), config policy fields + telemetry counters REMAIN_
- [~] **GPU-6** `XL` — Hardware backend real decode/encode/composite PATHS (cuda/vaapi/qsv/metal)  ·  _deps: GPU-1, GPU-3_  · _red `556b2d9`+`4ca50a4` → green `ae213c8` + my fix `8eaa293` (orchestrated, reviewed, me-verified + **HARDWARE-VALIDATED on the box**): the GPU-free host-side seam the real hardware paths consume — `multiview-ffmpeg::hwdecode` (`HwInputCodec`, `cuvid_name()` always-available libav `*_cuvid` mapping, `cuda`-gated `cuvid_decoder` selector that names NOTHING without `cuda` — no silent NVDEC, matching the encoder policy; ffmpeg-gated `select_decoder`/availability probe) + `multiview-compositor::native` admission seam (`is_native_vendor`, `admit_native_composite` rejecting cross-vendor/software/portable). **VALIDATED on real CUDA 12.6 (RTX 4060):** `cargo build -p multiview-ffmpeg --features cuda` exit 0 + `cargo test --features cuda` 10+2+… pass incl. `cuvid_decoder_offers_the_nvdec_name_only_with_cuda`. TDD note (disclosed): the green commit refined a red assertion — the red assumed `cuvid_decoder` was always-available; the impl correctly made it `cuda`-gated + added `cuvid_name()`, so the final tests are STRONGER (they assert the no-NVDEC-without-cuda policy), reviewed + accepted. **[~] honest:** this is the host-side plumbing + naming, hardware-COMPILE-validated; the actual GPU-resident decode→composite→encode OUTPUT (real nvdec frames, SSIM/PSNR vs CPU reference, nvenc round-trip) is **GPU-6b** (needs a real run on the box, in progress)._
- [~] **GPU-6b** `XL` — Real GPU decode→composite→encode OUTPUT validated on the box  ·  _deps: GPU-6_  · _**GPU ENCODE validated on real hardware** (`8593723`): fixed two cli feature-wiring bugs (presets omitted the cli's own `ffmpeg` feature → no run path; `nvidia` omitted `multiview-ffmpeg/cuda` → NVENC selector compiled out), so `multiview run --features nvidia` on gpu-test-box (RTX 4060/CUDA 12.6) now selects `encoder="h264_nvenc"`, never falters, and ffprobe confirms a real h264 (Main, 1920x1080) HLS stream — genuine GPU hardware H.264 NVENC encode end-to-end (was silently mpeg2). **Remaining:** the wgpu/native GPU COMPOSITE path is not yet wired into the cli run (still the CPU reference compositor), nvdec DECODE of real sources (the run used synthetic test sources), and the SSIM/PSNR GPU-vs-CPU parity + a long soak. Also: vaapi/videotoolbox ENCODE selection isn't modeled in `candidate_encoders` (only nvenc/gpl/lgpl) — a follow-on for the VAAPI/Apple presets._

### SUR — Captions · NMOS · web codegen

- [x] **SUR-1** `M` — IS-05 scheduled activation (absolute + relative)  ·  _deps: —_
- [x] **SUR-2** `L` — IS-07 MQTT broker transport  ·  _deps: SUR-1_  · _codec+topics+bounded drop-oldest queue (always-built) + live `rumqttc` client behind `is07-mqtt`; round-trip exercised against an in-process `rumqttd` broker_
- [x] **SUR-3** `XL` — Caption ingest Phase 2/3: broaden native decode beyond HLS WebVTT  ·  _deps: —_  · _`734e693`: native TEXT-caption decode to `CaptionCue::Text` proven via real LGPL libav decoders (SubRip/SRT, WebVTT-in-container w/ markup-strip, MOV_TEXT/tx3g) + ns rebase + empty/invalid-UTF-8 fail-safe; `decode_bytes_for_window`. CEA-608/708 (cc_dec + A53_CC side-data) + ASS + cli reader-wiring completed via SUR-3b + SUR-3c (both [x]): all caption source forms—CEA-608/708, ASS, MOV_TEXT, SubRip, Teletext, DVB-sub, WebVTT—now decode and wire into the per-tile CueStore._
  - [x] **SUR-3b** `L` — Caption decode remainder: CEA-608/708 (`cc_dec` over `AV_FRAME_DATA_A53_CC` side-data) + ASS, in multiview-ffmpeg  ·  _deps: SUR-3_  · _`b49bbc8`: `extract_a53_cc` (safe `frame.side_data(A53CC)`, before NV12 — swscale drops side-data) + `decode_video_frame` with `cc_dec real_time=1` (REAL bug found: cc_dec buffers to flush otherwise) → `CaptionCue::Text`; A53 fixture generator + tests (608 CC1; 708-service untested). cli caption-reader/PMT/HLS-SUBTITLES wiring = **SUR-3c** (cli, CORE)_
  - [x] **SUR-3c** `M` — cli caption-reader wiring for the broadened decoders: route in-container A53/text/ASS caption packets (PMT-walk discovery, HLS SUBTITLES-group resolve) through `decode_video_frame`/`decode_bytes_for_window` into the per-tile `CueStore`  ·  _deps: SUR-3b_  · _`94c6aef` (PR #44): `DecodedVideoFrame.a53_cc` (A53 bytes off the raw frame before NV12 conversion, before swscale); `embedded_cc_channel()`/`incontainer_caption_source()` route the declared selector to the correct decoder (dvbsub→bitmap, ass/ssa→ASS, subrip/srt→SubRip, mov_text/tx3g→MovText, dvb_teletext→Teletext); `InContainerSubRoute` (generalised from `DvbSubRoute`) + `EmbeddedCcRoute` wired in `open_and_stream` + `wire_source_captions`; `publish_window_cues` handles text and bitmap paths. TDD: `embedded_cc_ingest` real-pipeline test over the A53 fixture recovers "HELLO WORLD" CEA-608 cue into the `CueStore`. Best-effort+isolated (inv #1/#2/#10). Completes the SUR-3b→end-to-end caption path._
- [x] **SUR-4** `M` — OpenAPI: annotate the layout/resource write ops so they enter the spec  ·  _deps: —_
- [x] **SUR-5** `M` — Web: replace the hand-written layouts wrapper with the generated client + wire deferred routes  ·  _deps: SUR-4_  · _generated openapi-fetch client; create/update/delete wired; tsc+eslint+build+76 tests green_
- [~] **SUR-6** `XL` — AsyncAPI generation + generated realtime envelope types (replace the hand-modelled envelope)  ·  _deps: SUR-4_  · _`bd1bd68`: AsyncAPI 3.0 generator + `xtask gen-asyncapi` + generated TS types (additive, idempotent, tested); envelope SWAP + serve `/asyncapi.json` + CI AsyncAPI-CLI validation are **SUR-6b** below_

#### Discovered follow-on slices (added during shipping — keep the plan complete)
- [x] **GPU-4b** `M` — Wire the overlay-image GPU dispatch into the compositor `composite()` (upload image cache layers + bind group + `OverlaySubpass` between composite and encode) + the GPU-vs-CPU SSIM/PSNR parity test (GPU-tagged runner)  ·  _deps: GPU-4_  · _`0cdd9bd`: `composite_with_overlays` uploads-once to a persistent Rgba8Unorm layer-array, binds binding-5, dispatches `OverlaySubpass` (premultiplied-over in linear, inv #8); `plan_image_uploads` seam + 4 CPU tests; SSIM/PSNR parity test `#[ignore]`-gated (Y SSIM≥0.98/PSNR≥38dB, GPU-runner-only). no-overlay path byte-unchanged. (GPU-4 stays [~] until real-GPU parity runs)_

#### Discovered on real hardware (the GPU test box — gpu-test-box, 2× NVIDIA + CUDA 12.6, ffmpeg 5.1)
- [x] **HW-0** — First-ever hardware-feature COMPILE validated on the box (2026-06-06): `cargo build -p multiview-cli` with `--features ffmpeg`, **`--features nvidia` (cuda+ffmpeg+wgpu)**, and `--features linux-vaapi` (vaapi+qsv+ffmpeg+wgpu) all exit 0 on real CUDA 12.6 + libav. No feature-gated compile rot in the hardware paths (CI is GPU-free, so this is the first time they ever compiled). `--features ffmpeg` test suite 8/8 software_run + 4/5 streaming_encode.
- [x] **ENG-1c** `S` — Make `live_blocked_sink_stays_bounded_and_never_stalls` (streaming_encode) deterministic across encoder speeds  ·  _deps: —_  · _green `c098770` (orchestrated, REVIEWER-APPROVED, me-verified): root-caused empirically (a fast-draining sink → 1 packet/tick; the old test relied on the real mpeg2 encoder saturating the bounded queue within a fixed tick budget — which the box's ffmpeg 5.1 doesn't in the window). Hardened to deterministically overflow regardless of encoder latency without weakening the never-stalls/bounded assertions. The GPU box (ffmpeg 5.1) found this — exactly the hardware-validation payoff._

#### Discovered follow-on slices (batch-8)
- [ ] **PRV-4b** `S` — cli degradation-loop glue: observe `Hysteresis`/the ladder and call `FocusGate::suspend()/resume()` on the preview rungs + `tracing` each preview adaptation (ADR-P003)  ·  _deps: PRV-4_  ·  _→ in scope of the **webrtc-full push** (ADR-0048/T014/0049/P006/W020)_
- [~] **PRV-1b** `XL` — Native str0m `WhepTransport` impl: real ICE/DTLS/SRTP behind a `webrtc-native` feature + env-gated DTLS-SRTP loopback test + ffprobe egress check; add str0m/ring to `deny.toml`  ·  _deps: PRV-1_  · _red `20911db` → green `9da6b17` + my doc-fix `<this>` (orchestrated, reviewer-APPROVED [~], me-verified): `Str0mWhepTransport` behind a NEW off-by-default `webrtc-native` feature — the FULL sans-IO SDP offer→answer negotiation (str0m-minted real ICE ufrag/pwd + a real self-signed DTLS sha-256 fingerprint folded into the answer) runs WITHOUT a socket and is CI-tested. `str0m 0.16.2` (MIT/Apache, optional; its closure resolves to MIT/Apache/ISC/BSD — uses **aws-lc-rs, NOT ring**, so the plan's "ring" premise was stale) added to deny.toml allow-list. I fixed the 2 blocking aspirational-comment findings (the loopback test was doc'd as "the real packet-exchange half completing the DTLS handshake/SRTP" but only binds a socket + gathers candidates). **Remaining → PRV-1c:** the live DTLS handshake against a real peer + `SampleFeed`→SRTP media egress + the ffprobe egress check (none CI-runnable; needs a peer)._  ·  _**Relocation (2026-06-10, ADR-0048):** the native transport (the optional str0m dep + `whep/native.rs` + the `webrtc-native` feature) moves out of `multiview-preview` into the new `crates/multiview-webrtc` (feature `native`, tests preserved/moved); preview keeps only the pure `webrtc` seam. PRV-1b completes by relocation under **WRTC-1** in the webrtc-full push (ADR-0048/T014/0049/P006/W020)._
- [ ] **PRV-1c** `XL` — Native WHEP live egress: DTLS handshake + SampleFeed→SRTP + ffprobe  ·  _deps: PRV-1b_  · _drive str0m's poll_output/handle_input loop against a real peer to complete DTLS, packetize the preview `SampleFeed` into RTP→SRTP, and verify the egress with ffprobe (env/loopback-gated, not CI). The negotiation + transport scaffold + drive_egress_once single-step exist (PRV-1b)._  ·  _→ in scope of the **webrtc-full push** (ADR-0048/T014/0049/P006/W020); the "needs a peer, not CI" premise is retired — a second str0m `Rtc` over UDP loopback is the CI test peer_
- [x] **SUR-6b** `M` — Swap web realtime consumers (`connection.ts`/`useEngineEvents.ts`) onto the generated envelope types + serve `/asyncapi.json` on the axum router  ·  _deps: SUR-6_  · _`7b9fd90`: GET /asyncapi.json served (embedded, route-tested) + web `LifecycleState`/`TileState` now from generated-types (91 web tests); the AsyncAPI-CLI **CI** validation step is a separate `.github` follow-on (fold into IN-7's CI work)_
- [ ] **IN-6b** `XL` — WebRTC ingest native engine: a concrete str0m-backed ICE/DTLS/SRTP `MediaEngine` behind a `webrtc-native` feature + env-gated loopback test + wire `WebRtcProducer` into the cli; Opus/VP8 depacketizers; add str0m/ring to `deny.toml`  ·  _deps: IN-6_  ·  _→ in scope of the **webrtc-full push** (ADR-0048/T014/0048/P006/W020; delivered as **WRTC-2** — the transport lives in `multiview-webrtc` not behind a preview feature; we answer H.264+Opus, no VP8 ingest depacketizer; str0m is already in deny.toml via PRV-1b)_
- [x] **ENG-3b** `S` — Wire the measured NTP/PTP status into the cli wall-clock badge  ·  _deps: ENG-3_  · _`96ec96c`: cli `ntp` feature + `MeasuredSystemWallClock` `reference()` (off-hot-path `adjtimex` via engine `sysref::live` + `classify_system` → `SelectedReference::to_time_ref`; None→assumed fallback); 4 fake-NtpQuery tests; verified default/ntp/overlay,ntp/overlay,ffmpeg,ntp_
- [x] **GPU-5b** `L` — GPU-5 remainder: off-hot-path placement controller in `multiview-engine` (EWMA sustained-overload SHED-vs-MIGRATE, make-before-break) + config policy fields + telemetry counters  ·  _deps: GPU-5_  · _`250034b`: `PlacementController::observe→{Hold,Shed,Migrate,Split}` (pure, reuses `Hysteresis`, anti-storm cooldown/budget/min-gain, 11 tests) + `multiview-config` `PlacementConfig`/`DevicePin`/`gpu_pin` + `multiview-telemetry` placement counters. Wiring proposals into the supervisor execution (make-before-break swap) = **GPU-5c**_
- [x] **ENG-4b** `M` — GPU load probe remainder: live `/proc/<pid>/fdinfo` enc/dec-util walk (sum own PIDs) + telemetry gauge registration  ·  _deps: ENG-4_  · _`4152955`: `FdinfoMediaTracker` two-snapshot diff → `DeviceLoad.enc/dec_util_frac` (pure parser tested on fixtures, live `/proc` walk gated); the existing ENG-4 gauges consume it. i915 PMU `perf_event_open` (needs unsafe → FFI shim) = **ENG-4c**_
- [ ] **GPU-5c** `L` — Wire `PlacementController` (GPU-5b) into the engine runtime: a `LoadPoller` poll thread publishing the `arc_swap` `Vec<DeviceLoad>` snapshot, execute `Migrate`/`Split` proposals through the make-before-break supervisor + scene-swap, call `PlacementCounters::record_*`, resolve config `DevicePin`→`multiview_hal::DeviceId`. Touches the protected output core.  ·  _deps: GPU-5b_
- [x] **ENG-4c** `S` — i915 PMU `perf_event_open` GPU-busy path behind a tiny FFI-shim leaf crate (deny+SAFETY, like `multiview-ntpsys`), feeding the same `DeviceLoad` enc/dec fields on Intel where fdinfo is unavailable  ·  _deps: ENG-4b_  · _`4a45c46`: NEW `multiview-i915pmu` FFI crate — 3 `// SAFETY:` `unsafe` syscalls (`perf_event_open`/`read`/`close`) + a hand-rolled correct `PERF_ATTR_SIZE_VER1` (72-byte) `perf_event_attr`; hal `i915-pmu` feature folds busy-ns→frac into `DeviceLoad`. Pure diff tested; live `perf_event_open` gated. Poller call = ENG-4 infra_
- [x] **GPU-1b** `L` — Wire the `ProgramEncoder` into the cli bake consumer (encode-once-mux-many)  ·  _deps: GPU-1_  · _`a1af76c`: consumer owns one `ProgramEncoder`; `RunnableOutput`/`build_outputs`/`run_one_output`→`PacketMuxSink`; `StreamingPacketSource`; test seam evolved frame→packet (no assertion weakened); per-sink encoders retired; aspirational comments fixed. Completes GPU-1 / inv #7_
- [x] **HLS-WEBVTT-ISO** `M` — Isolate WebVTT subtitle renditions from the main HLS video demuxer (ADR-T011)  ·  _deps: —_  · _shipped #33 (test `f14afcb` → green `6f5c136`, ADR-T011): an HLS master carrying an `EXT-X-MEDIA:TYPE=SUBTITLES` WebVTT rendition (e.g. ABC News AU) could black out the whole tile — libav folds the rendition into the one shared `AVFormatContext`, so a corrupt/404/expired `.vtt` aborts the open or makes `av_read_frame` return that rendition's error for the entire context. Multiview already ingests HLS WebVTT via a separate isolated reader (`captions.rs::read_captions`, own context). Fix (4 parts): (b, primary) `multiview-ffmpeg` safe `discard_unrouted_subtitles(input, keep)` marks every `medium==Subtitle` stream `AVDISCARD_ALL` (one raw field write in the crate's `unsafe=deny` island — ffmpeg-next 8.1 `StreamMut` has no `set_discard`) except a routed `keep`, wired into `open_and_stream` before the first read (audio renditions never touched); (a) `ingest_open_options` HLS-master open-time hardening (strict=normal so libav drops the rendition pre-probe, seg_max_retry, sane protocol allowlist); (c) main-source recovery unchanged (supervised reconnect); (d) `caption_loop` becomes a supervised reconnect loop mirroring `ingest_loop` (live HLS rendition backs off + retries on EOF/error; finite VOD plays once). `crates/multiview-cli/tests/hls_webvtt_isolation.rs` green; inv #1/#2/#10. (Discovered during the #33 session — was not separately enumerated.) **FFmpeg-8.x follow-up fix shipped PR #47** (`3c96a48`+`7055a92`, hardware-validated on 8.1.1): FFmpeg 8.x removed the `strict` rendition gate that the original fix relied on, so the fix needed a second layer — pin the MAIN demuxer to a VIDEO variant playlist (no SUBTITLES rendition) via `resolve_hls_variant_url` (decode-at-display-resolution selection, inv #6/ADR-E001); a second commit resolved relative child URIs against the post-redirect (Akamai CDN) effective base URL (`FetchedText.url` via `av_opt_get location`) so CDN-redirected masters work correctly. ADR-T011 corrected to reflect both layers._
- [x] **SOAK-IPTV** `M` — `cargo xtask soak-iptv` quirk-tagged iptv-org test-source selector  ·  _deps: —_  · _shipped #33 (test red `b903318` → green `31b339b`): `xtask::iptv` builds an adversarial set of REAL test sources from iptv-org so the ingest path systematically hits resilience edge cases (the pinned ABC News WebVTT class, geo-blocked, part-time, header-gated/non-TLS origins, resolution extremes). Pipeline: fetch streams.json + channels.json → JOIN → quirk-classify → deterministic seed-stratified quirk-aware over-sample (in-crate SplitMix64, no RNG dep) → liveness probe (keeps first K live, deliberately retains a few dead/geo for the LIVE→STALE→RECONNECTING→NO_SIGNAL state machine) → filter nsfw + blocklist → emit a quirk-tagged manifest to the gitignored `.multiview-build/` (always incl. the pinned `https://c.mjh.nz/abc-news.m3u8`). All network behind `SourceCatalog`/`Prober` async traits; offline tests inject `FixtureCatalog`/`FixtureProber` (RFC-2606 example.* domains, no sockets); the real HTTP `HttpCatalog`/`HttpProber` (`ureq` 3.x, rustls) compile ONLY under the off-by-default `net` feature so the default build + `cargo deny` stay light/LGPL-clean. 12 integration + 2 unit tests green; real stream URLs are NEVER committed (resolved live per run). The automated "run each source + assert the state machine" soak is the documented box-gated follow-up. (Discovered during the #33 session — was not separately enumerated.)_

#### Discovered items (post-session, shipped in PRs #40–#49 — 2026-06-10)
- [x] **M10-ALARM** `M` — X.733 alarm state machine + config-probe → analyser → per-tick driver  ·  _deps: —_  · _shipped PR #45 (test red `da30e09` → green `6317b4a` + reconcile `af81988`): `multiview-engine` gains a per-tick `AlarmStateMachine` implementing the ITU-T X.733 severity lifecycle (CLEARED/INDETERMINATE/WARNING/MINOR/MAJOR/CRITICAL transitions, hysteresis) wired into `EngineRuntime::run_*_with_control` so an alarm fires on every output tick without touching the hot path; `multiview-config` gains a `FaultAnalyser` seam (`fault_probes`) that categorises config items into `ProbeResult`s; `config_probe_feeds_alarm_state_machine` + `alarm_reconcile` tests green. The engine fault analyser (config → analyser → alarm driver) is fully end-to-end: config-level faults surface as X.733 alarms in the event stream without blocking the output clock (inv #1/#10). Alarm emission is off-hot-path (non-blocking `EnginePublisher::publish_event`)._
- [x] **DEPLOY-CAPTIONS** `S` — Ship the native caption path (`overlay` feature) in the hardware deploy presets (nvidia/apple/linux-vaapi)  ·  _deps: —_  · _shipped PR #49 (test red `fde2d00` → green `1117507`): the three hardware Cargo feature presets (`nvidia`, `apple`, `linux-vaapi`) omitted the `overlay` feature despite caption/overlay being a first-class capability; the fix adds `multiview-cli/overlay` to each preset so `multiview run --features nvidia` (and the others) actually compiles the caption ingest and burn-in path. CI guard: `tests/deploy_presets.rs::deploy_presets_carry_captions` asserts each preset's feature list contains `overlay`. No change to the caption logic itself._
- [x] **WEBUI-DOCS-OVERHAUL** `L` — Professional kind-specific management UI + searchable in-app docs (ADR-W015/ADR-W016)  ·  _deps: CTL-3, SUR-4_  · _shipped PR #48 (commits `29d0a37` + `ba756b9` + `bbc9c73` + `bd3ffef`): (ADR-W015) kind-specific sources/outputs/overlays management forms (RTSP/HLS/NDI/SRT/RTMP source forms, file/HLS/RTSP/NDI/SRT/RTMP output forms, text/image/clock overlay forms), live status display, apply-layout trigger, preset selector, config export — all wired to the OpenAPI-generated client; output id namespace preserved on edit; overlay kind-scoped params; unknown-kind edit refused. (ADR-W016) searchable in-app documentation rendered from Markdown, linked from the nav. Adversarial-review fixes (`bbc9c73`+`ba756b9`): B1 (export base overlay) + M1 (output id namespace) + M2 (semantic validation) + M3 (precondition order) + several minor tightening. ADR-W015 + ADR-W016 added to `docs/decisions/`. SPA build + tsc + eslint green._
- [x] **ADR-BATCH-40-43** `S` — Four new docs-only ADRs pinning load-bearing contracts (PRs #40–#43)  ·  _deps: —_  · _shipped in four separate PRs: **ADR-R010** (PR #41, `05b814d`+`34d201b`) — make-before-break parallel-output migration primitive; corrected to clarify RT-12 bridge is NEW work, keepalive sink makes SWAP zero-new-encode. **ADR-T012** (PR #42, `5c46a28`) — reference-clock/wallclock source-selection contract. **ADR-T013** (PR #43, `ed8fec3`, renumbered from T012 after conflict) — shared RTP-audio → AudioStore rebase seam. **ADR-I005** (PR #40, `a436c36`+`2087f49`) — packet carrier (RT-12); corrected to reflect both `EncodedPacket` carriers live + fix RT-13→RT-12. All four docs-only; no code change._

#### WRTC — Full WebRTC push (brief: [webrtc](../research/webrtc.md); ADR-0048 · ADR-T014 · ADR-0049 · ADR-P006 · ADR-W020 — subsumes PRV-1c/PRV-4b/PRV-5b/IN-6b above; AUD-4b's Opus program rendition is partially delivered by WRTC-3)

- [ ] **WRTC-1** `XL` — Shared WebRTC transport endpoint: new leaf crate `crates/multiview-webrtc` (feature `native`, str0m =0.16.2 pinned) — ONE `WebRtcEndpoint` per process on a single dual-stack `[::]` UDP socket (`webrtc.udp_port`, default 8189) muxing ALL sessions (WHIP ingest · WHEP preview · WHEP output viewers · WHIP push) by STUN ICE-ufrag/learned-addr; full-ICE with IPv6-first host candidates + `webrtc.advertised_addresses`; per-run self-signed `DtlsCert`; one bounded driver task (recv → `handle_input`, drain `poll_output()`; inv #10); session GC (`session_idle_timeout` 30 s, tombstone eviction) + `max_sessions` (64); relocate preview's `webrtc-native`/str0m (`whep/native.rs`) here; str0m↔str0m UDP-loopback handshake + media-flow tests run in CI; `examples/whip_publish.rs` (ADR-0048)  ·  _deps: PRV-1b_
- [ ] **WRTC-2** `XL` — WHIP ingest end-to-end (RFC 9725): `SourceKind::Webrtc {token, audio}` + `POST /api/v1/whip/{source_id}` routes (OPTIONS/POST 201+Location/DELETE/PATCH→405, Bearer, 409 single-publisher, 503+Retry-After capacity, RFC 9457) over a native-free `WhipProvider` seam; RTP-mode session → `MediaEngine` → keyframe-gated H.264 depacketizer → NEW packet-fed multiview-ffmpeg H.264 decoder (decode-at-display-res, VUI→`ColorInfo`) → `TileStore` via `IngestPump` (`WrapBits::Rtp32`) + Opus depacketize→decode→`AudioStore` de-embed (ADR-T013 path); event-driven PLI (session start + after loss while the keyframe gate is closed, ≥2 s rate-limited) + documented OBS publish profile; `drive_webrtc` ingest supervision riding the tile state machine; fixes `WebRtcProducer.to_produced` (geometry from decoder/SPS) (ADR-T014; subsumes IN-6b)  ·  _deps: WRTC-1, IN-6_
- [ ] **WRTC-3** `XL` — WebRTC program outputs: `Output::Webrtc` (WHEP serve at `POST /api/v1/whep/{output_id}`, fanout `PacketSink` route()+1 encode-once, SPS/PPS cache + prepend at IDR, H.264 + B-frames-off validated at config time, rate-limited (≥2 s) force-IDR seam on the program encoder, `max_viewers`→503) + `Output::WhipPush` (WHIP client: Bearer, 307/308-redirect-follow — https-only, depth-capped, headers preserved — `a=setup:actpass` with the answerer picking the DTLS role, supervised reconnect like RTMP/SRT push) + the ONE program Opus rendition (post-loudnorm `AudioBlock` → bounded ring → single Opus encode, AUD-4b shape) fanned to all WebRTC consumers; engine-protected, never shed, bounded per-viewer drop-oldest rings (ADR-0049)  ·  _deps: WRTC-1_
- [ ] **WRTC-4** `L` — SPA WebRTC preview & management: `<WhepPlayer>` (injected `RTCPeerConnection` factory, recvonly video+audio, `muted`+`playsinline`, POST after ICE-gathering-complete or 2 s, relative-`Location` resolve, keepalive DELETE teardown) + `GET /api/v1/preview/capabilities` probe + WHEP→JPEG fallback ladder with an honest fallback badge (JPEG poll extracted into a reusable hook); mounts on MonitoringPage program card / input focus dialog / NEW outputs preview / LayoutEditorPage; `webrtc` source form (derived WHIP URL + token copy) + `webrtc`/`whip_push` output forms (ADR-W015 round-trip); in-app docs + glossary + latency row + i18n (en/ar/pseudo); vitest PC-factory fake + MSW, Playwright fallback e2e, real-browser hardware validation (ADR-W020)  ·  _deps: WRTC-2, WRTC-3_

### DEV — Managed devices & display out (briefs: [display-out](../research/display-out.md) · [managed-devices](../research/managed-devices.md); ADR-0044/0045, ADR-M008..M011, ADR-RT007, ADR-W017)

> **Hardware gating (updated 2026-06-10):** two NDI-licensed ZowieBox units and a
> multi-generation Cast fleet are available in the test environment, and the HP t630 test unit is
> approved as a dedicated display-node target (its incumbent signage stack may be taken over) —
> those validation legs are unblocked. Still gated: the Raspberry Pi legs (a Pi 4 is being
> provisioned; no Pi 5 exists). The build itself was never blocked.

- [ ] **DEV-A1** `M` — `multiview-config`: `Device` + `SyncGroup` types, validation, export semantics (ADR-M008 lands with it)  ·  _deps: —_
- [ ] **DEV-A2** `S` — `multiview-events`: `Topic::Devices` + device/timing event types (ADR-RT007)  ·  _deps: —_
- [ ] **DEV-A3** `L` — `multiview-control`: devices + sync-groups stores, CRUD routes, OpenAPI, audit, bare-verb actions (ADR-W017)  ·  _deps: DEV-A1, DEV-A2_
- [ ] **DEV-A4** `XL` — `zowietek` driver: typed HTTP client + poller actor + three facets + device state machine (feature-gated; socket-free tests) (ADR-M009)  ·  _deps: DEV-A3_
- [ ] **DEV-A5** `M` — Discovery infra: mDNS browse (new — none exists in-repo) + `/discovery/devices` endpoints + untrusted-inventory confirm-adopt flow  ·  _deps: DEV-A3_
- [ ] **DEV-A6** `L` — SPA: DevicesPage, adopt flow, detail tabs, "From device" pickers, help pages  ·  _deps: DEV-A3, DEV-A4_
- [x] **DEV-B1** `XL` — `multiview-output/src/display/` behind `display-kms`: drm-rs sink + mailbox/atomic-flip loop + TEST_ONLY probe + EDID/forced-mode policy; `Output::Display` wired through the five config matches + cli `build_outputs` + SPA form (ADR-0044)  ·  _deps: — (1-day fence spike first)_
- [ ] **DEV-B2** `L` — HAL/placement scanout affinity: KMS connector discovery in probe, sink-locality constraint in select, placement migration gate  ·  _deps: DEV-B1_
- [ ] **DEV-B3** `L` — Render path: wgpu NV12→XRGB into a GBM/dmabuf scanout buffer (AMD/fallback) + NV12 direct scanout (Intel/Pi) + the wgpu version-pin decision  ·  _deps: DEV-B1_
- [ ] **DEV-B4** `L` — ALSA HDMI audio sink: ELD gating, hdmi:/vc4 card config, buffer-level servo + adaptive resampler  ·  _deps: DEV-B1_
- [ ] **DEV-B5** `L` — `multiview node` subcommand: single-ingest display-node mode + systemd unit + bare-metal/container deploy (kernel-uevent listener; rootless polling fallback) (ADR-0045)  ·  _deps: DEV-B1, DEV-B2, DEV-B3, DEV-B4_
- [ ] **DEV-B6** `M` — Node enrollment/pairing + `displaynode` driver + Display-tab assignment + wall-head binding  ·  _deps: DEV-A3, DEV-B5_
- [ ] **DEV-C1** `L` — Outbound presentation epoch: per-program `WallClockRef` on the control WS + RTCP SR on RTSP + `EXT-X-PROGRAM-DATE-TIME` on HLS + optional RFC 7273 SDP attrs (ADR-M010)  ·  _deps: DEV-A2_
- [ ] **DEV-C2** `M` — Node presentation discipline: epoch+link-offset frame chooser at vblank + skew telemetry  ·  _deps: DEV-B5, DEV-C1_
- [ ] **DEV-C3** `M` — Sync groups: apply/measure/test-pattern, weakest-member tier computation, drift alarms  ·  _deps: DEV-A3, DEV-C2_
- [ ] **DEV-C4** `M` — Clock-layer telemetry + deployment guidance: ptp4l/chrony configs, servo offset export, acceptance-soak harness (soak is hardware-gated)  ·  _deps: DEV-C2_
- [x] **DEV-D1** `S` — CORS on the HLS HTTP endpoints (standalone; benefits all browser consumers)  ·  _deps: —_  · _shipped (red `e05394c` → green): there was NO Rust HLS HTTP surface at all (disk files + a header-less nginx sidecar), so the slice ships the delivery surface itself — `multiview_output::hls::http::hls_router` (ADR-0032 §6: per-extension Content-Type incl. `.m4s`, Cache-Control tiers, Accept-Ranges/206/416, extension-allowlist + traversal-safe) behind ONE Origin-reflecting CORS layer (`with_hls_cors`: reflect Origin + `Vary: Origin` always, OPTIONS→204, no-Origin→no CORS headers), mounted per HLS/LL-HLS output at `/hls/{output-id}/` on the control listener (`hls_mounts` + new `multiview_control::serve_router`; unauthenticated like `/docs` — inv #10 safe, file reads only) + the reference nginx fronting config (`deploy/hls-nginx.conf`) carrying the identical contract. 10 oneshot/property tests + a real-socket cross-origin e2e green._
- [ ] **DEV-D2** `L` — Cast spike (`rust_cast` vs hand-rolled prost/tokio-rustls) → session actor + `cast` driver (ADR-M011)  ·  _deps: DEV-A3, DEV-A5, DEV-D1_
- [ ] **DEV-D3** `M` — SPA cast flow: ad-hoc sheet, ephemeral sessions, save-as-device, latency badge, help page  ·  _deps: DEV-D2_
- [ ] **DEV-D4** `M` — Cast hardware validation across ≥2 device generations + firmware-pinned notes (**gated on hardware acquisition**)  ·  _deps: DEV-D2, DEV-D3_

### SYN — Clock & timer synthetic sources (extends ADR-0027; brief [clock-timer-sources](../research/clock-timer-sources.md); [ADR-0047](../decisions/ADR-0047.md))

> Pure-Rust, default-build (no `ffmpeg`/GPL); the clock/timer **render** is `overlay`-gated like the as-built clock (`synth.rs:150`). All time is integer/`chrono` (inv #3/#6), wall-clock is **sampled** not pacing (inv #1), the disciplined reference is display-only (ADR-T012). Builds on the as-built `clock_face` SDF primitives + `TextEngine` runs + NV12/linear bake — **no new renderer**.

- [ ] **SYN-CLOCK-1** `M` — Pure tz resolver + dual-face/metadata model: `chrono`+`chrono-tz` dep (deny-clean), `resolve_offset(Tz, WallTime) -> TimeZoneOffset` (DST-correct per displayed instant), `UTC±HH:MM` badge formatter; golden tests at DST boundaries  ·  _deps: —_
- [ ] **SYN-CLOCK-2** `S` — Config: `ClockFaceConfig::Dual` + `Clock` fields (`timezone`, `label`, `show_offset`, `show_reference`, `numerals`); validation (unknown IANA id → typed `ConfigError`; `timezone` wins over `tz_offset_minutes` + warn); round-trip TOML/JSON  ·  _deps: SYN-CLOCK-1_
- [ ] **SYN-CLOCK-3** `M` — Renderer: `dual` placement (`clock_face` upper + centred readout lower) + metadata strip (label + offset/reference badges) in `render_clock`; per-render tz resolution wired into `SyntheticKind::Clock` + `render_key` field index; content-aware goldens  ·  _deps: SYN-CLOCK-2_
- [ ] **SYN-TIMER-1** `M` — Pure timer math: `TimerTarget` next-occurrence resolution (time-of-day across midnight/DST; absolute datetime), `displayed_seconds` (down/up), at-target clamp (`hold`/`continue`/`zero_then_up`/`recur`), duration decompose + `TimerFormat` (incl. frames from the cadence `Rational`, drop-leading-zero `auto`); property + golden tests  ·  _deps: SYN-CLOCK-1_
- [ ] **SYN-TIMER-2** `S` — Config: `SourceKind::Timer` + `TimerTarget`/`TimerDirection`/`TimerOnTarget`/`TimerFormat` enums (`#[serde(flatten)]` target tagged on `target`, distinct from `kind`); validation + round-trip TOML/JSON  ·  _deps: SYN-TIMER-1, SYN-CLOCK-2_
- [ ] **SYN-TIMER-3** `M` — Renderer + generator: timer render (centred mono run + overrun prefix/badge) + `SyntheticKind::Timer` resolved descriptor wired into `generator_loop` (field-index `render_key`; `_ff` bakes at cadence); content-aware goldens (count, at-zero, overrun)  ·  _deps: SYN-TIMER-2, SYN-CLOCK-3_
- [ ] **SYN-CLOCK-UI** `L` — SPA + spec: source-kind picker entries for the extended clock + new timer; IANA zone dropdown from `chrono_tz::TZ_VARIANTS`; clock/timer forms; OpenAPI/JSON-schema regen; `examples/clock-timer.toml`; in-app docs  ·  _deps: SYN-CLOCK-3, SYN-TIMER-3_


---

## Part 3 — Detailed work items

_Each item: Goal · Touches · Approach · Acceptance · Risks · Read‑first. Flip the box above and set Status here as items land._


## AUD — Audio pipeline + tone

**Grounding summary.** The runtime pipeline (`crates/multiview-cli/src/pipeline.rs`) is **video-only** today: `Pipeline::build` (pipeline.rs:483) builds per-source video `IngestPlan`s, the engine emits one NV12 canvas per tick, and the egress (`StreamEgress::spawn`, pipeline.rs:1570) fans each `Arc<Nv12Image>` to sinks whose `run()` registers exactly **one** video stream (`FileSink::run` sink.rs:301, `drive_to_single_muxer` sink.rs:402). Verified: `demo-output/clocks/program.ts` has one `mpeg2video` stream, no audio. The pure-Rust building blocks already exist and are untouched by the runtime: `Mixer` (mixer.rs — program bus + discrete tracks + silence-fill), `LoudnessMeter` (loudness.rs — full BS.1770-4 M/S/I/LRA/dBTP), `AudioFileDecoder`/`Resampler` (decode.rs / multiview-ffmpeg resample.rs, 48k fltp canonical), and `AudioEncoder`/`AudioEncodeTarget` (multiview-ffmpeg encode.rs:151 — opened-and-ready but never wired to a muxer). `Muxer::add_stream` (mux.rs:91) already accepts a second stream. The synth `generator_loop` (synth.rs:307) produces video frames only. There is **no** `AudioCodec` logical selector mirroring `VideoCodec` (codec.rs) — that is the first gap.

---

### `[ ]` AUD-1 — Logical audio-codec selector + license-aware resolution · effort: M · deps: none
- **Goal:** Add an `AudioCodec` logical type and `select_audio_encoder` to `multiview-ffmpeg` so the pipeline can resolve AAC/Opus/MP2 the same license-aware way video already resolves via `select_encoder`, keeping the default build LGPL-clean.
- **Touches:** `crates/multiview-ffmpeg/src/codec.rs` (mirror `VideoCodec`/`select_encoder` codec.rs:40-157); `crates/multiview-ffmpeg/src/encode.rs:308` (`static_codec_name` already lists `"aac"`); `crates/multiview-ffmpeg/src/lib.rs` re-exports.
- **Approach:**
  1. Add `enum AudioCodec { Aac, Opus, Mp2 }` with `lgpl_software_encoder()` returning `"aac"` (FFmpeg's native AAC is LGPL), `"libopus"` (LGPL), `"mp2"` (LGPL) — all default-buildable; reserve any GPL/`libfdk_aac` (nonfree) behind a gate exactly as `gpl_software_encoder()` does for video (codec.rs:73).
  2. Add `candidate_encoders(AudioCodec)` + `select_audio_encoder(AudioCodec) -> Option<&'static str>` walking the same fixed-policy order.
  3. Extend `static_codec_name` (encode.rs:308) for `"libopus"`/`"mp2"`.
- **Acceptance (done when):** unit tests in codec.rs assert `AudioCodec::Aac.lgpl_software_encoder() == Some("aac")`, Opus→`libopus`, Mp2→`mp2`, and that `libfdk_aac` is never returned in a default build (mirror the H.264-returns-None tests codec.rs:183). `cargo test -p multiview-ffmpeg` green; guardrail: no `unwrap`/`as` in non-test code.
- **Risks/notes:** Native `aac`/`libopus`/`mp2` are all LGPL — keep them in the default candidate list; only `libfdk_aac` is nonfree and must stay gated. CI has FFmpeg present (video tests already encode), so the audio encoders should `find_by_name` fine; if a build lacks `libopus`, `select_audio_encoder` must fall through (don't `expect`).
- **Read first:** ADR-R005 (decode→re-encode normalized, capability matrix); `crates/multiview-ffmpeg/src/codec.rs` header doc.

### `[~]` AUD-2 — Per-source runtime audio decode thread (peer of video ingest) · effort: L · deps: AUD-1
- **Goal:** Decode each file/URL source's audio on its own thread into a per-source lock-free audio store (48k fltp), so the output clock can *sample* audio per tick exactly as it samples video tiles — never pacing or stalling the engine (#1/#10).
- **Touches:** new `crates/multiview-cli/src/audio.rs` (peer of `synth.rs`); `IngestPlan` (pipeline.rs:421) gains an optional audio route; `IngestSupervisor::start` (pipeline.rs:930) spawns the audio thread alongside the video decode thread. Reuse `multiview_audio::AudioFileDecoder` (decode.rs:48) and `multiview_ffmpeg::Resampler` (resample.rs:54).
- **Approach:**
  1. Build an `AudioTileStore` analogous to the video `TileStore` (a bounded SPSC ring of `AudioBlock`s keyed by source id, drop-oldest) — or reuse `multiview_framestore` generically if its store is type-parametric; keep it lock-free and read-only on the engine side per ADR-R006.
  2. In a new `audio_decode_loop` (model on `synth::generator_loop` synth.rs:307: spawn, `stop: &AtomicBool`, prompt teardown), pull `DecodedBlock`s, PTS-rebase to the program clock, and on EOF/dropout publish `AudioBlock::silence` (mixer.rs already silence-fills, but the *store* must also never gap — load-bearing for invariant A per ADR-R005 §4.1).
  3. Wire into `IngestSupervisor::start` so audio threads are torn down on `supervisor.shutdown()` (pipeline.rs:1073) exactly like video.
- **Acceptance (done when):** new test feeds a short fixture clip (multiview-ffmpeg `test_fixtures.rs`) and asserts the store yields 48k fltp blocks then silence past EOF; a "dead source" test asserts the audio thread never blocks join past one chunk (reuse the synth `sleep_until` teardown pattern). Invariant re-assert: engine loop only *samples* the audio store (#10 no back-pressure). Guardrail: `unsafe_code = forbid` preserved (multiview-audio touches libav only via the safe seam, decode.rs header).
- **Risks/notes:** Live URL/NDI/audio-free sources contribute no decoder → ride silence-fill (the build-time meter timelines already handle this absence, pipeline.rs:384). CI: needs an audio-bearing fixture; generate a tone clip rather than relying on network. Resample drift handled later (ADR-T006 is the soft-resample story; v1 may use fixed 48k and note the drift gap).
- **Read first:** streaming-gotchas §5 (long-run drift) + §7 (A/V sync/jitter); ADR-R005 §4.1; core-engine §9.2 audio bullet.

### `[ ]` AUD-3 — Program-bus mix + per-tick sample budget on the output clock · effort: L · deps: AUD-2
- **Goal:** At each output tick, pull exactly `samples_per_tick = 48000·den/num` samples per track, mix the program bus via the existing `Mixer`, and carry the audio alongside the canvas through the egress — making the output truly encode-once-mux-many on the audio side.
- **Touches:** `StreamItem` (pipeline.rs:1397) gains an `audio: ProgramAudio` field (program-bus `AudioBlock` + per-discrete-track blocks); the hot-loop projection `state_of` (pipeline.rs:979) samples the audio stores and runs `Mixer::mix_program` (mixer.rs:148); `StreamEgress`/`StreamingFrameSource` (pipeline.rs:1570, run_one_output:1729) carry the audio to sinks.
- **Approach:**
  1. Construct one `Mixer` (mixer.rs:60) per run at the canonical 48k/stereo format; `add_input` + `route_to_program` per source from the routing config (AUD-7).
  2. In `state_of`, after sampling video, pull `samples_per_tick` from each audio store, `Mixer::submit` each, call `mix_program()` → the program `AudioBlock`; attach to `StreamItem`. The mix is cheap and pure; it runs on the hot loop but does no I/O and cannot block (the stores are non-blocking) — assert this is within tick budget.
  3. Extend the egress channel payload from `Arc<Nv12Image>` to a `(Arc<Nv12Image>, ProgramAudio)` (or a small struct) so each sink receives matched A/V per tick. Honour the same drop-on-overload/block-for-exact `SendPolicy` (pipeline.rs:122) — audio rides the same bounded queue, dropped *with* its frame so A/V stay locked.
- **Acceptance (done when):** test drives N ticks and asserts `total_audio_samples ≈ ticks · samples_per_tick` (continuity — the audio analogue of "output-clock never stalls"); a dropped-source test asserts the program bus is gap-free silence, never absent. Invariant #1 re-assert: audio sample count is a pure function of tick count; #10: mixing never back-pressures the engine. ffprobe check deferred to AUD-4.
- **Risks/notes:** `samples_per_tick` is fractional for 1001-denominator rates (e.g. 30000/1001 → 1601.6 samples/tick) — accumulate the remainder across ticks (1601/1602 alternation) so the long-run sample count stays exact (this is the audio side of the drift invariant). Guardrail: no float `as` truncation on the sample count — use rational accumulation.
- **Read first:** streaming-gotchas §0 (unified timing, input-PTS→output-frame-index) + §7; ADR-R005 §4.1.

### `[ ]` AUD-4 — Audio encode + dual-stream mux in the output sinks (the core gap) · effort: XL · deps: AUD-1, AUD-3
- **Goal:** Register a *second* (audio) stream on every muxer and interleave encoded audio packets with video — so `program.ts`/HLS/RTMP/SRT carry video **and** audio, completing encode-once-mux-many. This is the load-bearing change that turns the verification (`program.ts` one video stream) into two streams.
- **Touches:** `crates/multiview-output/src/sink.rs` — `EncodeConfig` gains an audio half (codec_name/sample_rate/layout/bitrate); `FileSink::run` (sink.rs:301), `drive_to_single_muxer` (sink.rs:402), `SegmentSink::run` (sink.rs:506), `PushSink::run` (sink.rs:632); a new `AudioFrameSource` trait paralleling `VideoFrameSource` (sink.rs:102); reuse `multiview_ffmpeg::AudioEncoder` (encode.rs:172) and `Muxer::add_stream` (mux.rs:91).
- **Approach:**
  1. Add an `AudioEncodeConfig` to `EncodeConfig`; in `run`, after `muxer.add_stream(video…)`, build `AudioEncoder::new(target)` (encode.rs:178) and `muxer.add_stream(audio.as_codec_context(), audio.time_base())` → audio `stream_index`. Write header once with both streams registered (it is immutable for the session — the pinning rule, ADR-R005/brief §3.3).
  2. Generalise `drive_to_single_muxer` to interleave: each tick, encode the video frame (existing path) **and** feed the tick's `AudioBlock` to `AudioEncoder::send_frame` (chunking to the encoder's `frame_size`, encode.rs:217, e.g. AAC's 1024). Re-stamp audio PTS from a sample counter (`audio_pts = Σ samples`), the audio analogue of `out_pts = tick` (#3 — never forward input PTS). Drain both encoders' packets and `muxer.write_packet` on the matching stream index; libav's interleaved writer orders by DTS.
  3. Carry the audio through `StreamingFrameSource`/`run_one_output` (pipeline.rs:1729) and the packet-fan path (`drive_packets_to_single_muxer` sink.rs:439) for the multi-sink case, so the *same* encoded audio packets fan to file+HLS+push (invariant #7 — audio is encoded once, not per output).
- **Acceptance (done when):** new integration test runs a `bars`+tone config and ffprobes the output: **two** streams — `video` + `aac` (or configured codec) — and `silencedetect` confirms the program is *not* silent (content-aware, not a byte hash). HLS segments each carry audio; `tsp`/ffprobe confirm continuity. Invariant #1: audio packet count tracks tick count with no gaps; #7: assert audio is encoded once even with 2+ sinks (count `AudioEncoder` builds). Name tests `program_ts_has_video_and_audio_streams`, `hls_segments_carry_audio`.
- **Risks/notes:** AAC frame_size (1024) ≠ samples_per_tick → maintain a per-sink sample ring that buffers across ticks and flushes whole encoder frames; flush remainder on EOF. Push/HLS interleaving must keep audio non-back-pressuring (#10) — a wedged audio encoder drops with its frame under live policy. Default codec MP2 or AAC (both LGPL); document MP2 as the most-compatible-LGPL-in-TS default. Guardrail: this crate already maps libav errors via `ff()` (sink.rs:137) — no `unwrap`.
- **Read first:** `crates/multiview-output/CLAUDE.md` (encode-once-mux-many, inv #3/#7); ADR-R005 §4.2 capability matrix (TS=N PIDs, HLS=renditions); brief §1.2 rule 5 (muxer continuity config).

### `[ ]` AUD-5 — `bars` synthetic-source 1 kHz tone companion · effort: M · deps: AUD-2, AUD-3
- **Goal:** Give the `bars` synthetic source a 1 kHz sine on its audio bus (the line-up tone companion to colour bars), so the synthetic line-up signal is audible and the audio path is exercisable without any external media.
- **Touches:** `crates/multiview-cli/src/synth.rs` (add a tone generator peer to `generator_loop` synth.rs:307); `SyntheticKind::Bars` (synth.rs:46); the audio store from AUD-2. Tone math is pure Rust → can live in `multiview-audio` (new `tone.rs`) so it is unit-testable and reusable, with synth.rs as the publish loop.
- **Approach:**
  1. Add `fn tone_block(format, freq_hz, phase, frames) -> (AudioBlock, next_phase)` generating a phase-continuous 1 kHz sine at a calibrated line-up level (−18 dBFS / −20 dBFS EBU alignment — make it a const) into `AudioBlock::from_interleaved` (format.rs:103). Phase carried across ticks so there is **no click** at tick boundaries.
  2. In synth, when `SyntheticKind::Bars` (and only bars — `solid`/`clock` stay silent), run a `tone_loop` peer publishing `samples_per_tick` into the source's audio store at cadence, mirroring `generator_loop`'s stop/teardown.
  3. Route the bars tone to the program bus by default (it is a real source's audio per ADR-R005 fan-out).
- **Acceptance (done when):** unit test asserts `tone_block` is phase-continuous across a block boundary (last sample of block N and first of N+1 are within one sample-step of the sine) and that an integrated `LoudnessMeter::momentary` (loudness.rs:217) on the tone reads the expected level ±0.5 LU; integration: a `bars`-only run's `program.ts` ffprobes a ~1 kHz tone (FFT bin or `astats`) — content-aware. Invariant #1: tone never gaps. Guardrail: sine via `f64::sin`, sample narrowing only through the clamped helper (mixer.rs:182 pattern), no raw `as`.
- **Risks/notes:** Keep tone generation pure/deterministic so it runs identically on CI without hardware. 1 kHz at 48k is exactly 48 samples/cycle → phase bookkeeping is clean; still carry phase as f64 for non-integer-divisor freqs.
- **Read first:** synth.rs header (synthetic source = peer of decode thread); ADR-0027 (synthetic sources, referenced in synth.rs:1); resilience-and-av §4.1.

### `[x]` AUD-6 — EBU R128 loudness normalisation on the program bus · effort: L · deps: AUD-3
- **Goal:** Normalise *only* the program bus toward a target LUFS (−23 broadcast / −16 web) with a true-peak ceiling, reusing the existing `LoudnessMeter` math, while leaving discrete tracks unaltered (authenticity guarantee, ADR-R005/R006).
- **Touches:** new `crates/multiview-audio/src/loudnorm.rs` (a normaliser built on `loudness.rs` measurement, loudness.rs:97); applied in the pipeline mix step (AUD-3, pipeline.rs `state_of`) between `mix_program` and the encoder.
- **Approach:**
  1. Implement a single-pass/live `loudnorm` (brief §4.1: dynamic mode, live tolerance ±1 LU, gate at −70 LUFS so a lost input's silence doesn't drag the target): drive gain from the running integrated/short-term loudness off `LoudnessMeter` (loudness.rs:223/266), smoothed, with a true-peak limiter clamping to −1.5 dBTP using `true_peak_dbtp` (loudness.rs:355).
  2. Run the meter **read-only** per ADR-R006 — but here it measures the *program bus we are about to emit*, so it is on the audio path, not the engine hot path; ensure it cannot stall the output (drop-and-continue, never block).
  3. Apply gain to the program `AudioBlock` only; discrete tracks bypass entirely (mixer.rs `discrete_track` mixer.rs:131 stays clean).
- **Acceptance (done when):** test feeds a known-loudness signal (the AUD-5 tone, or a louder/quieter fixture) and asserts the normalised program-bus integrated loudness converges to target within ±1 LU and true-peak never exceeds −1.5 dBTP; a discrete-track test asserts bytes are byte-identical to input (unaltered). Invariant: −70 LUFS gate excludes a silenced input. Re-assert #1 (normaliser can't stall the clock) and #10. Name `program_bus_converges_to_target_lufs`, `discrete_tracks_unaltered`.
- **Risks/notes:** True-peak is the expensive metric (ADR-R006: ~2.5-3× cost, worse on ARM) — only run the 4× oversampled TP detector on the program bus (one track), which AUD-6 already scopes. Live single-pass loudnorm cannot match file-mode ±0.2 LU — document ±1 LU. Guardrail: gain math in f64, narrow via the clamp helper.
- **Read first:** ADR-R006 (read-only metering, normalize only the bus, true-peak gating); resilience-and-av §4.1 + §5.

### `[x]` AUD-7 — Audio routing config schema + capability validation · effort: M · deps: AUD-3, AUD-4
- **Goal:** Add the `{input_id, channels, target_track, language, title, include_in_program_bus, gain, mute}` routing model to config and validate it against the per-output capability matrix (TS=N, HLS=select-one, RTMP=endpoint-gated, NDI=channel-map), so routing is declarative and impossible selections are rejected at config time.
- **Touches:** `crates/multiview-config/src/schema.rs` (new `AudioRoute`/audio block on `Source` or top-level, alongside `Output` schema.rs:508 which currently carries only a *video* `codec`); `crates/multiview-config/src/error.rs` (new validation errors); the `Mixer` wiring in AUD-3 reads these routes.
- **Approach:**
  1. Add an `audio` section: per-source `AudioRoute` fields per ADR-R005 §4.1; add an `audio_codec` token to each `Output` variant (schema.rs:512) resolved via AUD-1.
  2. Implement a machine-readable `OutputCapability` matrix (brief §10 "first-class data structure, not scattered conditionals") and a validator that rejects e.g. N discrete tracks on legacy RTMP, maps NDI to channel-map, and flags HLS as select-one.
  3. Feed routes into `Mixer::add_input`/`route_to_program`/gain (mixer.rs:76-95) and per-track mute in AUD-3.
- **Acceptance (done when):** config tests parse a multi-track routing doc and assert the `Mixer` is wired with the right gains/program membership; capability tests assert "N tracks on RTMP" is rejected (or degraded with an explicit warning) while "N PIDs on TS" passes — the designed-in asymmetry from brief §9.4. Round-trip serde test (the schema is `Serialize+Deserialize`, schema.rs:509). Guardrail: no panic on malformed routes — return `ConfigError`.
- **Risks/notes:** Keep the capability matrix machine-readable for reuse by the WebUI (AUD-8). Don't over-build NDI/RTMP multitrack now (those sinks aren't runnable yet, pipeline.rs:2719) — validate + degrade honestly.
- **Read first:** ADR-R005 (capability matrix, routing data model §4.1-4.2); resilience-and-av §10 data-model anchors (`AudioRoute`, `OutputCapability`).

### `[ ]` AUD-8 — WebUI audio routing matrix + loudness meters · effort: L · deps: AUD-7, AUD-6
- **Goal:** Expose the routing matrix (program bus + discrete tracks, per-input gain/mute/include) and live program-bus loudness compliance in the SPA, with the capability-aware validator greying out impossible selections per output.
- **Touches:** `web/src/pages/` (new `AudioPage.tsx` peer of `MonitoringPage.tsx`); `web/src/resources/api.ts` (extend the Outputs bindings api.ts:295 with audio routes/codec); `web/src/realtime/` (loudness over the existing engine WebSocket, useEngineEvents.ts) reusing `multiview_audio::meterdata`/`Conflator` (loudness/meterdata, ~30 Hz) per ADR-R006.
- **Approach:**
  1. Add the routing-matrix editor (input × output-track grid) driven by AUD-7's capability matrix — disable cells the matrix forbids (brief §8: "greys out N-track audio on a legacy-RTMP endpoint, shows channel-map for NDI").
  2. Wire program-bus M/S/I/LRA/dBTP + clip flags over the existing realtime WebSocket at 10-25 Hz, binary/numeric, ballistics applied client-side (ADR-R006 wire-to-browser). Reuse the existing meter-timeline infrastructure already feeding per-tile overlays (pipeline.rs:384).
  3. Follow the existing page/i18n/test conventions (the pages have `.test.tsx` peers, e.g. `AlarmsPage.test.tsx`).
- **Acceptance (done when):** component test renders the matrix and asserts a forbidden cell (RTMP multitrack) is disabled and NDI shows "channel-map"; a meter component test asserts LUFS/dBTP render from a mock WS frame. Existing `web` test suite (vitest) green. No new engine-path code — UI only consumes telemetry (#10).
- **Risks/notes:** Telemetry must stay numeric-only over WS (never audio) — bandwidth + ADR-R006. Defer simultaneous multi-track *monitoring* on HLS (it is select-one) to a track *selector* per the brief.
- **Read first:** resilience-and-av §8 (web surface: audio routing matrix, metering) + §5 (wire-to-browser); ADR-R006.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs
- /workspaces/mosaic/crates/multiview-output/src/sink.rs
- /workspaces/mosaic/crates/multiview-audio/src/mixer.rs
- /workspaces/mosaic/crates/multiview-ffmpeg/src/encode.rs
- /workspaces/mosaic/crates/multiview-cli/src/synth.rs


## SYN — Clock & timer synthetic sources (extends ADR-0027; brief [clock-timer-sources](../research/clock-timer-sources.md); [ADR-0047](../decisions/ADR-0047.md))


### `[ ]` SYN-CLOCK-1 — Pure tz resolver + dual-face/metadata model · effort: M · deps: none
- **Goal:** Add the missing **upstream offset resolver** the clock model already anticipates (`clock.rs:57` "DST resolved upstream to a concrete offset") so an IANA `timezone` renders a DST-correct offset for the *displayed* instant, plus the formatters the dual-face + metadata need — all pure, deterministic, injected-time.
- **Touches:** workspace `Cargo.toml` + `crates/multiview-cli/Cargo.toml` (add `chrono` + `chrono-tz`, `default-features = false`); a new pure helper (in `multiview-config` or a `multiview-overlay::tz` module) `resolve_offset(tz: chrono_tz::Tz, at: WallTime) -> TimeZoneOffset`; a `format_utc_offset(TimeZoneOffset) -> String` (`UTC±HH:MM`). Reuses `multiview_overlay::clock::{WallTime, TimeZoneOffset}` (clock.rs:30/:57).
- **Approach:**
  1. Add the deps; run `cargo deny check` (chrono/chrono-tz closure is `MIT OR Apache-2.0`/`Unicode-3.0`, all on `deny.toml:65`); commit `Cargo.lock`.
  2. `resolve_offset`: `tz.offset_from_utc_datetime(DateTime::from_timestamp(at.unix_seconds(),0).naive_utc()).fix().local_minus_utc() / 60` → `TimeZoneOffset::from_minutes(…)`. No `as`-cast — narrow via `i32::try_from`/clamped helpers (guardrail).
  3. `format_utc_offset`: integer h/m from the signed minutes, `UTC` for zero — never float.
- **Acceptance (done when):** golden tests: `Australia/Sydney` at `2026-01-15T00:00:00Z` ⇒ `+11:00`, at `2026-07-15` ⇒ `+10:00`; `America/New_York` across spring-forward/fall-back instants; `UTC` ⇒ `+00:00`; `format_utc_offset` round-trips the badge text. `cargo deny check` green. Guardrail: pure (no `SystemTime::now`), integer math, no panic on any `Tz`.
- **Risks/notes:** Bound binary size with `chrono-tz` `default-features=false` (+ `filter-by-regex` if needed). Keep the resolver pure so the control plane can reuse it for the form preview. The bundled tz db means a tz-rule change needs a dep bump (documented in the brief §9 / ADR-0047).
- **Read first:** brief [clock-timer-sources](../research/clock-timer-sources.md) §5; [ADR-0047](../decisions/ADR-0047.md); `multiview-overlay/src/clock.rs` header + `:57`.

### `[ ]` SYN-CLOCK-2 — Config: `Dual` face + clock metadata fields · effort: S · deps: SYN-CLOCK-1
- **Goal:** Extend `SourceKind::Clock` additively (`schema_version` unchanged) with `ClockFaceConfig::Dual`, `timezone` (IANA), `label`, `show_offset`, `show_reference`, `numerals`, validated and serde-round-tripping across TOML+JSON.
- **Touches:** `crates/multiview-config/src/schema.rs:197` (`ClockFaceConfig::Dual`) + `:225` (the `Clock` variant fields per brief §6.1); `crates/multiview-config/src/error.rs` (unknown-IANA-id `ConfigError`); the example-validation test.
- **Approach:**
  1. Add the variant + fields exactly as brief §6.1 (`#[serde(default, skip_serializing_if = "Option::is_none")]` on the `Option`s; `Dual` is `#[non_exhaustive]`-safe).
  2. Validate `timezone` against `chrono_tz::Tz::from_str` at load → typed `ConfigError`; if both `timezone` and a non-zero `tz_offset_minutes` are set, accept with a `tracing::warn!` and let `timezone` win (documented precedence).
- **Acceptance (done when):** round-trip TOML+JSON for `face="dual"` + every new field; an unknown IANA id is a typed error (no panic); both-present emits the warning and resolves to `timezone`; `examples/*.toml` still parse. Guardrail: no `unwrap` on parse; serde tagged (no `untagged`).
- **Risks/notes:** Keep `tz_offset_minutes` working (legacy fixed offset). `Dual` must default-skip nothing existing.
- **Read first:** brief §6.1; conventions §5 (serde policy); `schema.rs:189-239`.

### `[ ]` SYN-CLOCK-3 — Renderer: dual placement + metadata strip + per-render tz · effort: M · deps: SYN-CLOCK-2
- **Goal:** Render `dual` (analogue face + digital readout) and the metadata strip (label + `UTC±HH:MM` + optional reference badge) onto the slate, resolving the IANA offset **per render** so a long-running clock follows DST.
- **Touches:** `crates/multiview-cli/src/synth.rs:150` (`render_clock` `dual` branch + strip), `:45/:72` (`SyntheticKind::Clock` carries `mode`, `tz: Option<Tz>`, `label`, `show_offset`, `show_reference`, `numerals`), `:138` (`render_key` → field index). Reuses `clock_face`/`ClockFaceStyle::at` (subpass.rs:328/:342) + `TextEngine::rasterize_run` (text.rs).
- **Approach:**
  1. `dual`: place `clock_face` via `ClockFaceStyle::at(cx, upper_cy, radius)` in the upper region; centre the digital run in the lower region (reuse the existing extent-measure/centre logic, synth.rs:210-244).
  2. Strip: two left/right-aligned runs (label, offset badge) in the bottom `~18%`; reference badge from a passed-in `TimeRef` (display only — never sampled into timing).
  3. Per render: `let zone = SyntheticKind::Clock.tz.map(|tz| resolve_offset(tz, now)).unwrap_or(fixed_offset);` so DST is live; `render_key` becomes the displayed-second (clock) field index.
- **Acceptance (done when):** content-aware goldens extending synth.rs:464 — `dual` differs from both `analog` and `digital` at the same instant and from the slate; a labelled clock differs from unlabelled; a `show_offset` Sydney clock's frame differs between a January and a July instant (badge text changed). Sample-based (`Nv12Image::sample`/`y_plane`), never an assertion-free hash. `overlay`-gated like the as-built tests.
- **Risks/notes:** Keep the bake NV12/linear (inv #5/#8) — only text placement is new. Cost stays one bake/visible-field change (inv #9). Reference badge is display-only (ADR-T012, inv #1).
- **Read first:** brief §2, §4, §5.2-5.3; `synth.rs:150-261` + `:464-556`; `subpass.rs:283-417`.

### `[ ]` SYN-TIMER-1 — Pure timer math: target resolution, direction, at-target, format · effort: M · deps: SYN-CLOCK-1
- **Goal:** All countdown/countup math, pure and deterministic (injected now + target + cadence): next-occurrence resolution, `displayed_seconds`, the at-target clamp policies, and the duration→string formats including frames from the cadence rational.
- **Touches:** a new pure module (in `multiview-overlay` or `multiview-config`): `resolve_target(TimerTarget, now: WallTime) -> i64` (target Unix), `displayed_seconds(direction, target, now, on_target) -> i64`, `format_duration(seconds: i64, sub_ns: i64, cadence: Rational, fmt: TimerFormat, overrun: …) -> String`.
- **Approach:**
  1. `time_of_day`: resolve the next (down) / most-recent (up) occurrence of `HH:MM:SS` in the zone, across midnight and a DST gap (`02:30` on a spring-forward day → the realised instant); `recur_daily` re-arms.
  2. `datetime`: parse `at` as a naive local datetime, attach the zone offset (DST-correct), → Unix.
  3. `displayed_seconds` = `target-now` (down) / `now-target` (up); apply `on_target` (`hold` clamps at 0; `continue`/`zero_then_up` allow negative→overrun; `recur` re-arms). Decompose abs seconds → d/h/m/s; frames = `floor(sub_ns * num / (den * 1e9))` (integer, no float); `auto` drops leading zero units.
- **Acceptance (done when):** golden: `14:29:55` vs target `14:30:00` ⇒ `00:00:05` (down); at-zero ⇒ `00:00:00`; one second past with `zero_then_up` ⇒ `+00:00:01`; `up` is the negative of `down` at the same instant; `recur_daily` re-arms to next day. Property (`proptest`): `format(decompose(n))` monotone in `n`, day field dropped iff zero, `_ff` frames in `0..fps`. Mutation: a flipped `Down`↔`Up`, a dropped clamp, an offset-sign error must fail. Guardrail: integer/`chrono` only, no float seconds (inv #3/#6).
- **Risks/notes:** Midnight/DST edge cases are the footguns — test them explicitly. Frames only meaningful for `_ff`; keep it integer division against the `Rational`.
- **Read first:** brief §3, §5.4; [ADR-0047](../decisions/ADR-0047.md) (Decision 4, alt F); `multiview-core::time::Rational`.

### `[ ]` SYN-TIMER-2 — Config: `SourceKind::Timer` + target/direction/policy/format enums · effort: S · deps: SYN-TIMER-1, SYN-CLOCK-2
- **Goal:** Add the `Timer` source variant + its enums per brief §6.2, internally tagged (target on `target`, flattened under `kind = "timer"`), validated and round-tripping.
- **Touches:** `crates/multiview-config/src/schema.rs` (new `SourceKind::Timer`, `TimerTarget`, `TimerDirection`, `TimerOnTarget`, `TimerFormat`); `error.rs` (bad `at` string / unknown zone / `recur` without `recur_daily` → typed errors).
- **Approach:**
  1. Add exactly the shape in brief §6.2; `TimerTarget` is `#[serde(tag="target", rename_all="snake_case")]` `#[serde(flatten)]`ed into the variant (two distinct tag keys — `kind` and `target` — no clash, same pattern as `Source.kind` flatten, schema.rs:332).
  2. Validate: `at` parses for the target kind; `on_target = recur` only with `time_of_day`+`recur_daily`; zone resolves (reuse SYN-CLOCK-2's validator).
- **Acceptance (done when):** round-trip TOML+JSON for both targets + every enum; `kind="timer", target="time_of_day", at="14:30:00"` parses; invalid `at`/zone/`recur` combos are typed errors (no panic); `examples/*.toml` parse. Guardrail: tagged serde, no `untagged`, no `unwrap`.
- **Risks/notes:** Double-tag (`kind` + `target`) is intentional — assert it in a test so a future refactor can't collapse them.
- **Read first:** brief §6.2 (incl. the tag-collision note); conventions §5.

### `[ ]` SYN-TIMER-3 — Renderer + generator: timer render + overrun + generator wiring · effort: M · deps: SYN-TIMER-2, SYN-CLOCK-3
- **Goal:** Render the timer (centred mono duration run + optional overrun prefix/badge) and wire `SyntheticKind::Timer` into the per-tick `generator_loop`, re-rendering once per displayed field.
- **Touches:** `crates/multiview-cli/src/synth.rs` (`render` arm for `Timer`, `SyntheticKind::Timer` resolved descriptor, `from_source_kind` mapping, `render_key` field index incl. `_ff` per-frame, `animated()==true`).
- **Approach:**
  1. Render: format the duration (SYN-TIMER-1) → centred `rasterize_run` (mono); when past target and styled overrun, prepend the prefix and draw the `OVER`/`ELAPSED` a11y badge run.
  2. Generator: resolve "now" from the sampled wall clock each iteration (synth.rs:322), compute `displayed_seconds`, key on the displayed field (`_ff` keys on the frame index, others on the second); republish last-good on a render miss (synth.rs:332).
- **Acceptance (done when):** content-aware goldens: `00:00:05` differs from `00:00:04`; the at-zero frame; an overrun frame differs from a pre-target frame (prefix/badge drew). Generator test: across simulated ticks a `down` timer's published frames change at field boundaries and hold otherwise. Invariants: #1 (never paces; PTS from the tick), #2 (holds last-good), #9 (one bake/field). `overlay`-gated.
- **Risks/notes:** `_ff` is the one cadence-rate baker — keep it opt-in and cheap (one text run). Never read wall clock outside the generator (tests inject `now`).
- **Read first:** brief §3.3, §4.2, §4.4; `synth.rs:107-344`.

### `[ ]` SYN-CLOCK-UI — SPA + spec surface for the extended clock + timer · effort: L · deps: SYN-CLOCK-3, SYN-TIMER-3
- **Goal:** Surface the new clock fields + the `timer` source in the WebUI source-kind picker/forms with an IANA zone dropdown, regenerate the OpenAPI/JSON-schema, ship an example, and document it in-app.
- **Touches:** `web/src/` source-kind picker + forms (clock `face`/`timezone`/`label`/`show_offset`/`numerals`; timer target/direction/on_target/format/label); zone dropdown from `chrono_tz::TZ_VARIANTS` (exposed via a `/capabilities/timezones` endpoint or a generated list); `cargo xtask gen-openapi`; `examples/clock-timer.toml`; in-app docs/concepts page.
- **Approach:**
  1. Extend the generated API bindings (the SPA client is OpenAPI-generated); add the clock/timer forms following the existing source-form conventions + `.test.tsx` peers.
  2. Populate the zone `<Combobox>` from the typed timezone list (don't hand-type IANA ids); show a live offset preview using the SYN-CLOCK-1 resolver via the API.
  3. Ship `examples/clock-timer.toml` (a world-clock-wall + a show-countdown) mirroring `examples/synthetic-sources.toml`.
- **Acceptance (done when):** component tests render the clock + timer forms and assert the zone dropdown + a forbidden combo (e.g. `recur` without `recur_daily`) is disabled/flagged; `tsc`/eslint/vitest green; OpenAPI regen committed; `examples/clock-timer.toml` parses in the example-validation test. UI-only — no engine-path code (inv #10).
- **Risks/notes:** Keep the zone list server-sourced/generated, not duplicated in TS. WCAG 2.1 AA on the new forms.
- **Read first:** brief §6.4, §9; conventions §6 (API) + §8 (frontend); `examples/synthetic-sources.toml`.


---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/synth.rs
- /workspaces/mosaic/crates/multiview-config/src/schema.rs
- /workspaces/mosaic/crates/multiview-overlay/src/clock.rs
- /workspaces/mosaic/crates/multiview-compositor/src/overlay/subpass.rs
- /workspaces/mosaic/examples/synthetic-sources.toml


## OUT — Output servers (RTSP, NDI)


### `[~]` OUT-1 — RTSP egress decision spike + sidecar baseline (MediaMTX) wired as a Push target · effort: M · deps: none
- **Goal:** Land a *working* RTSP egress immediately via the ADR-0006 sidecar fallback (publish the existing encoded stream to MediaMTX over libav RTSP), and lock the in-process-vs-sidecar decision before committing to the GStreamer C-stack, so RTSP output stops being a no-op without dragging GLib into the lean default build.
- **Touches:** `crates/multiview-cli/src/pipeline.rs:2713` (replace the warn-and-skip arm); `crates/multiview-output/src/sink.rs:550-573` (`PushProtocol::Rtsp` already maps to the `rtsp` muxer — reuse it); `crates/multiview-config/src/schema.rs:514` (`Output::RtspServer { mount, codec, latency_profile }`); a new `docs/decisions/ADR-0006` follow-up note (read-only here — propose, don't write).
- **Approach:**
  1. Confirm `PushProtocol::Rtsp` → `muxer_name() == "rtsp"` (sink.rs:570) drives `Muxer::create_as(rtsp://host:8554/<mount>, "rtsp")` (mux.rs:70) — i.e. libav RTSP ANNOUNCE/RECORD to a listening MediaMTX. This is the *publish hop* ADR-0006 calls the sidecar baseline; it needs **zero new sink code**, only routing.
  2. In `build_outputs` (pipeline.rs:2683), replace the `Output::RtspServer { .. }` skip arm with a `RunnableOutput::Push { sink: PushSink::new(cfg, PushProtocol::Rtsp, rtsp_publish_url(mount)), label: "rtsp" }`, deriving the publish URL from a `[system.rtsp] publish_base` config (default the MediaMTX `rtsp://127.0.0.1:8554`). Keep the connect-failure tolerance already in `run_push_output` (pipeline.rs:1789) so a missing sidecar is reported, never fatal (#1/#10).
  3. Add a `deploy/` MediaMTX sidecar manifest entry (there is a `deploy/` dir already) so `rtsp` output has a peer; gate behind config, not a Cargo feature (no native dep).
  4. Spike-document: prototype `appsrc ! h264parse ! rtph264pay` caps against real NVENC output is the in-process path (OUT-2); record the go/no-go in the ADR follow-up.
- **Acceptance (done when):** new test `crates/multiview-output/tests/push_rtsp_ffprobe.rs` (model on `push_udp_ffprobe.rs`) that spawns a local RTSP listener (MediaMTX in CI service container, or `ffmpeg -rtsp_flags listen` as the loopback peer), runs the `Rtsp` `PushSink`, then `ffprobe rtsp://…` re-reads the stream and asserts codec/geometry + exact frame count; assert `PushProtocol::Rtsp.muxer_name() == "rtsp"` (pure, always-CI). Invariant re-assert: the RTSP push reuses `drive_to_single_muxer` so it is the *same* one-encode stream (#7); a dropped/absent RTSP peer is logged and dropped, never back-pressures the engine (#10) and never stalls the output clock (#1) — proven by the existing tolerant `run_push_output` path.
- **Risks/notes:** Sidecar needs a MediaMTX binary/service in CI (network + extra process) — guard the live test behind a `requires-mediamtx` cfg like the existing RTMP/SRT tests already self-exclude. LGPL-clean: no GPL codec, no GLib pulled — this baseline keeps the default build native-light. No `unwrap`/`as`/indexing in non-test (the URL builder must use checked formatting).
- **Read first:** ADR-0006; core-engine §9.2 (RTSP server bullets); streaming-gotchas §4 (pacing — never flush a backlog to a server).

### `[~]` OUT-2 — In-process RTSP server via `gst-rtsp-server`, fed pre-encoded NALs (`PacketSink`) · effort: XL · deps: OUT-1
- **Goal:** Deliver the ADR-0006 *primary* path — an in-process RTSP server that fans the already-encoded canvas (encode-once-mux-many) to RTSP clients with **no GStreamer re-encode** — so RTSP is a first-class low-latency egress without a Go sidecar hop.
- **Touches:** new `crates/multiview-output/src/rtsp/` module behind a new `rtsp` Cargo feature in `crates/multiview-output/Cargo.toml:features` (mirror the `ndi`/`ffmpeg` gating pattern at lines 62-72); implement `fanout::PacketSink` (fanout.rs:105) so it slots into the existing `PacketRouter`; new dep `gstreamer-rtsp-server` (not yet in `Cargo.lock`); `crates/multiview-cli/src/pipeline.rs:2713` to register the server sink when `--features rtsp`.
- **Approach:**
  1. Add the `rtsp` feature pulling `gstreamer` + `gstreamer-app` + `gstreamer-rtsp-server` (LGPL-2.1, dynamic-link). Run the GLib main loop on its own thread bridged to Tokio (ADR-0006 consequence).
  2. Build the factory pipeline exactly as core-engine §9.2 specifies: `appsrc name=src is-live=true format=TIME ! h264parse ! rtph264pay name=pay0 config-interval=-1` (and `h265parse ! rtph265pay` for HEVC), `factory.set_shared(true)` so one encode fans to all clients.
  3. Implement `RtspServerSink: PacketSink` whose `deliver(&Arc<EncodedPacket>)` (fanout.rs:111) pushes the **pre-encoded** NAL bytes (`EncodedPacket.data`, fanout.rs:97) into `appsrc` with the packet's already-tick-stamped `pts`/`duration` (fanout.rs:89) — strictly non-blocking, into a bounded drop-oldest buffer (the module contract, fanout.rs:16-21).
  4. Resolve the upstream-feed mismatch: the CLI currently fans **baked NV12 frames** to per-sink encoders (`StreamingFrameSource`, pipeline.rs:2797), not encoded packets. For RTSP we must feed *encoded* packets. Two options grounded in existing code: (a) register the server under `PacketRouter` fed from the CLI's single encoder's packet stream (the encode-once path `PacketMuxSink` already consumes via `PacketSource`, sink.rs:782), or (b) front the server with one `PushSink`-style encoder. Choose (a) to honour #7 (one encode). This requires the CLI to expose its encoder's packet stream to the router — coordinate with the engine stream.
  5. Prototype the exact `appsrc` caps (stream-format/alignment/SPS-PPS, `config-interval=-1` for late joiners) against real NVENC/VideoToolbox byte-stream output — ADR-0006 flags this as the must-validate risk.
- **Acceptance (done when):** `tests/rtsp_server_playout.rs` (feature `rtsp`) starts the server, a client (`ffprobe`/`gst-launch rtspsrc`) pulls `rtsp://127.0.0.1:8554/<mount>` and asserts codec/geometry + a monotonic, gap-free frame run; a second simultaneous client proves `set_shared(true)` fan-out from one encode (#7); a deliberately-slow/abandoned client must not stall the others or the producer (#1/#10) — assert the bounded drop-oldest buffer sheds rather than blocks `deliver`. Unit test: `PacketSink::deliver` returns without blocking under a full buffer.
- **Risks/notes:** Pulls the GStreamer/GLib C stack + a GLib main loop — **must** stay behind the `rtsp` feature so the default build is unaffected (ADR-0006 consequence; lean-static-binary tension). CI needs GStreamer base+good plugins (`rtph264pay`, `h264parse`) installed — gate the live test on their presence. LGPL-clean: gst-rtsp-server is LGPL-2.1 and dynamic-linked (clean); the H.264 *encode* stays in our LGPL/gpl-codecs-gated encoder, GStreamer only *payloads*. Guardrails: the FFI surface is GStreamer's safe Rust bindings — keep `unsafe_code = forbid` in this crate, no `unwrap`/`as`/indexing in non-test.
- **Read first:** ADR-0006; core-engine §9.2 (the exact `appsrc`/`rtph264pay`/`set_shared` recipe); `multiview-output/CLAUDE.md` (#7/#10 contract); streaming-gotchas §4.

### `[x]` OUT-3 — NDI dynamic-load backend (`NDIlib_v6_load`) + feature/license scaffolding · effort: L · deps: none
- **Goal:** Stand up the ADR-0008 runtime-loaded NDI backend — feature-gated, never-vendored, dynamically loaded via `NDIlib_v6_load()` with mandatory attribution and a runtime license-acceptance gate — so the default open-source build carries zero proprietary code/obligations and NDI stays inert until an operator accepts.
- **Touches:** `crates/multiview-output/Cargo.toml` (the `ndi` feature is currently an empty stub at line 72 — wire it to `grafton-ndi` + `libloading`); new `crates/multiview-output/src/ndi/` module; `crates/multiview-config/src/schema.rs:550` (`Output::Ndi { name }`) plus a `[system.ndi] accept_license` setting (ADR-0008 runtime gate — add to the system schema); attribution surfaced in `NOTICE`/`README` (read-only here — propose).
- **Approach:**
  1. Wire the `ndi` feature to `grafton-ndi` (build-links the dylib) **and** provide the `NDIlib_v6_load()` libloading path so the SDK is resolved at *runtime*, not build time (ADR-0008: grafton-ndi's `build.rs` panics without the SDK; the dynamic-load backend keeps the default build SDK-free). Headers MIT-vendored, runtime never vendored.
  2. Implement the runtime license gate: an `NdiLicense` guard that refuses to construct any NDI sender/receiver until `[system.ndi] accept_license = true` (audited who/when) — no NDI I/O starts otherwise (ADR-0008 hard requirement). Surface a clear refused-status string, never a panic.
  3. Add the mandatory attribution constants (`"NDI® is a registered trademark of Vizrt NDI AB"`, ndi.video link) exposed for the About/NOTICE surfaces.
  4. Keep the whole module behind `#[cfg(feature = "ndi")]`; the default `cargo test`/`cargo deny` (all-features=false) never sees it — matching the `ffmpeg`-feature isolation already used (Cargo.toml comments lines 16-31).
- **Acceptance (done when):** `tests/ndi_license_gate.rs` (feature `ndi`) asserts that with `accept_license = false` no sender is constructed and a typed refusal is returned (never a panic, never a started sender); a unit test asserts the attribution string constants are present; `cargo deny check` with all-features=false shows **no** new dependency entered the default graph (the LGPL-clean baseline is intact). Loader test: `NDIlib_v6_load()` resolution failure (no runtime present) surfaces a typed error, not a crash.
- **Risks/notes:** NDI SDK is proprietary/royalty-free — CI/Docker for the `ndi` feature must *fetch* the SDK (not vendored); the default CI must **not**. Runtime needs a resolvable dylib or the dynamic-load path. Attribution + no-"NDI"-in-product-name obligations are mandatory (ADR-0008). Guardrails: libloading is `unsafe` FFI — confine all `unsafe` to a thin `multiview-ffmpeg`-style boundary or to grafton-ndi; keep `multiview-output` `unsafe_code = forbid` by isolating the loader. No `unwrap`/`as`/indexing in non-test.
- **Read first:** ADR-0008; core-engine §10 (NDI integration & licensing); `docs/io/ndi.md`.

### `[~]` OUT-4 — NDI output Sender wired as a Sink (host-memory copy from canvas) · effort: L · deps: OUT-3
- **Goal:** Publish the composited multiview as a single NDI source (one `NDIlib` Sender) fed from the canvas, wired into the CLI's sink fan-out exactly like the file/HLS/push sinks, so NDI output is a real, runnable egress (gated + license-accepted).
- **Touches:** `crates/multiview-cli/src/pipeline.rs:2719` (replace the warn-and-skip `Output::Ndi` arm); a new `RunnableOutput::Ndi` variant (pipeline.rs:326) + its `run_one_output` arm (pipeline.rs:1733); `crates/multiview-output/src/ndi/` Sender from OUT-3; reuses `StreamingFrameSource`'s baked `Arc<Nv12Image>` fan-out (pipeline.rs:2785).
- **Approach:**
  1. Unlike RTSP/HLS/push, NDI takes **uncompressed** frames, so feed it from the *baked NV12* fan-out (`StreamingFrameSource`, pipeline.rs:2797) — not the encoded packet router. This is the one sink that is correctly frame-fed, not packet-fed; #7 still holds (composite once; NDI is a separate uncompressed rendition, explicitly allowed when the codec differs).
  2. Convert NV12 → NDI `UYVY` (`color_format_fastest`, core-engine §9.2/§10) at the NDI boundary — ADR-0008 notes NDI frames are host-memory, so the one host↔GPU copy lives here; reuse the existing plane-copy discipline (`copy_plane`, pipeline.rs:2856).
  3. Add `RunnableOutput::Ndi { sink, name }`; in `build_outputs` push it (gated on `cfg(feature = "ndi")` *and* license-accepted), else keep the honest skip. Its sink runner (`run_one_output`) pulls baked frames off the bounded fan-out channel and `send_video` — a slow/absent NDI receiver paces only this consumer (#10), never the engine.
  4. Re-stamp NDI frame timecode from the tick counter, never raw input PTS (#3), consistent with every other sink (sink.rs:23-28).
- **Acceptance (done when):** `tests/ndi_output_roundtrip.rs` (feature `ndi`, license-accepted) creates a Sender named from config and an NDI Finder/Receiver in-process, then asserts the advertised source appears and a received frame matches the canvas geometry/format and frame count; a content-aware check on the pixel values (a known ramp survives NV12→UYVY). Invariant re-assert: NDI sink runs off the hot path on its own thread (pipeline.rs:1581 model), so a stalled receiver cannot back-pressure the engine (#10) or stall the output clock (#1); timecode is tick-derived (#3).
- **Risks/notes:** Needs a resolvable NDI runtime + network in CI for the live roundtrip — gate it behind the `ndi` feature *and* a `requires-ndi-runtime` cfg (like the RTMP/SRT live tests self-exclude); the construction/license-gate path stays always-testable. NDI is proprietary/opt-in — never in the default build. Full NDI is ~125–250 Mbps/1080p60 and CPU-decoded downstream (core-engine §10 density note) — document the network budget. Guardrails: the NV12→UYVY conversion and host-copy must be checked-indexing only (mirror `copy_plane`), no `unwrap`/`as`/indexing in non-test.
- **Read first:** ADR-0008; core-engine §9.2 (NDI out bullet) + §10; `docs/io/ndi.md`.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs (skip points at :2713 RTSP, :2719 NDI; `build_outputs` :2683; `RunnableOutput` :326; `run_one_output` :1729; sink fan-out `StreamEgress` :1562)
- /workspaces/mosaic/crates/multiview-output/src/sink.rs (`PushSink`/`PushProtocol::Rtsp` :550-655; `drive_to_single_muxer` :402; `PacketMuxSink`/`PacketSource` :720-808)
- /workspaces/mosaic/crates/multiview-output/src/fanout.rs (`PacketSink` :105, `PacketRouter` :117, `EncodedPacket` :84 — the encode-once-mux-many seam the RTSP server plugs into)
- /workspaces/mosaic/crates/multiview-output/Cargo.toml (`ndi` empty-stub feature :72; add `rtsp` feature; gating pattern :62-72)
- /workspaces/mosaic/crates/multiview-config/src/schema.rs (`Output::RtspServer` :514, `Output::Ndi` :550; add `[system.ndi] accept_license`)


## IN — Inputs (NDI · ST 2110 · WebRTC · YouTube)

Grounded in: `crates/multiview-input/src/{source.rs,st2110/transport.rs,webrtc/transport.rs,error.rs}`, `crates/multiview-cli/src/pipeline.rs` (ingest_plan_for @2909, ingest_loop @3225, open_and_stream @3285, SourceLocation @3071), `crates/multiview-config/src/schema.rs` (SourceKind @211), ADR-0008, ADR-0015, docs/io/{ndi,youtube-live}.md, docs/research/streaming-gotchas.md §0/§5/§7.

Key architectural fact established by the scaffold: every ingest path must produce frames into a `TileStore` via a `source::FrameProducer` (`crates/multiview-input/src/source.rs:104`) driven by an `IngestPump`, OR via the CLI's direct decode loop (`open_and_stream`). The pump is *sampled, never pacing* (invariants #1/#10). All four items converge on emitting `ProducedFrame`s through that trait or the CLI store, never blocking the engine.

---

### `[x]` IN-1 — ST 2110 receive: frame assembler over the depacketizers · effort: M · deps: none
- **Goal:** Add a pure, testable per-frame assembler that turns the stream of `V20Payload` SRD segments + RTP marker bits into a single `ProducedFrame`, so the `st2110` transport has something to feed the `IngestPump` (the current `RtpReceiver` yields raw packets with no reassembly).
- **Touches:** `crates/multiview-input/src/st2110/` — new `assembler` submodule alongside `v20.rs`/`v30.rs`/`v40.rs`; consumes `RtpHeader.marker`/`.timestamp` (`rtp.rs:73/81`) and `SrdSegment` (`v20.rs:77`). Pure, always-compiled (mirrors how depacketizers are always compiled per `Cargo.toml` `st2110` feature comment).
- **Approach:**
  1. Define `FrameAssembler` keyed by RTP timestamp: accumulate `V20Payload` segments into a line-addressed pixel buffer until the RTP `marker` bit (RFC 4175 end-of-frame) flips, then emit a complete raster.
  2. Map the 90 kHz RTP `timestamp` (`rtp.rs:81`) to `raw_pts` in the producer's timebase; set `wrap_bits` = 32-bit RTP (the `WrapBits` enum in `normalize.rs` already has an RTP case — confirm and reuse). Surface `discontinuity` on a sequence gap reported by the ST 2022-7 reconstructor.
  3. Emit `ProducedFrame { pixels, raw_pts, discontinuity, meta }` with `meta.format` per the -20 sampling (8/10-bit 4:2:2 → the canonical NV12/P010 the compositor expects; document the conversion as a follow-up if the pgroup unpack isn't trivial — keep this item to luma+chroma plane assembly).
  4. Errors → `Error::St2110` (`error.rs:53`).
- **Acceptance (done when):** new test `crates/multiview-input/tests/st2110_assemble.rs` (with the mandatory `#![allow(clippy::unwrap_used,…)]` header per AGENTS.md) builds golden multi-packet frames (marker on the last) and asserts one `ProducedFrame` per marker with correct geometry and monotonic `raw_pts`; a property test asserts no panic on truncated/out-of-order/duplicate sequences. Re-assert #1 (assembler never blocks; an incomplete frame at EOS is dropped, never awaited). No `unwrap`/`as`/indexing in non-test (the codebase uses `read_u16`/`saturating_add` helpers — follow that).
- **Risks/notes:** pure logic, runs in the default LGPL-clean build (no feature gate, no NIC). Watch `as_conversions` ban on the 90 kHz→ns rebase — push the float-free math into `multiview_core::time::Rational`.
- **Read first:** docs/research/streaming-gotchas.md §0 (PTS pipeline) + §7 (RTP reorder/jitter); ADR-T003.

### `[x]` IN-2 — ST 2110 receive: wire `RtpReceiver`/`DualPathReceiver` into a `FrameProducer` + PTP timing · effort: L · deps: IN-1
- **Goal:** Make the compile-only `st2110::transport` an actual ingest `Source` by driving the sockets → assembler → `IngestPump`, with receive timing anchored to a PTP/ST 2059 reference (or wall-clock fallback) per the timing model.
- **Touches:** `crates/multiview-input/src/st2110/transport.rs` (current `RtpReceiver::recv_rtp` @~95, `DualPathReceiver::recv_merged` @~140); a new `St2110Producer` implementing `source::FrameProducer`; `crates/multiview-input/Cargo.toml` `st2110` feature (already `["tokio/net"]`).
- **Approach:**
  1. Build an async receive task: loop `DualPathReceiver::recv_merged`, extracting `V20Payload`/`V30Payload`/`V40Payload` (the `extract` closure already exists), feeding IN-1's assembler; completed frames go through a bounded `tokio::sync` channel to a `FrameProducer::next_frame` adapter (the `IngestPump` is sync — bridge with a non-blocking `try_recv`, newest-wins, never block: invariant #10).
  2. Add `St2110Config` (local addr, optional multicast group + interface, optional path-B for 2022-7, declared fps/timebase) and a `bind`/`spawn` entry. Reuse `join_multicast_v4` (@~58).
  3. PTP timing: per streaming-gotchas §5 the master clock stays `CLOCK_MONOTONIC`; do **not** slave the output to the input. Use the RTP 90 kHz media clock for per-input PTS only; expose a hook for a future PTP epoch but anchor first frame to `master_now` exactly like `IngestPump::pump_one` already does. (Document that full ST 2059 lock is out of scope; cite the "free-run the rest" guidance.)
  4. Keep everything `#[cfg(feature = "st2110")]`; the producer adapter trait impl can be pure.
- **Acceptance (done when):** loopback integration test (gated `#[cfg_attr(not(feature="st2110"), ignore)]` and behind a `--ignored`/env guard) binds two `UdpSocket`s on `127.0.0.1`, sends hand-built golden -20 datagrams from a second socket through both paths, and asserts the `TileStore` receives the reassembled frame with merged/de-duped sequences; `cargo check --features st2110` and `cargo clippy --features st2110 -- -D warnings` are green (today's CI clippy job at ci.yml:32 runs **default features only**, so the gated transport is currently never clippy-checked — see IN-6). Re-assert #10 (channel is bounded, drop-oldest; a stalled reader never back-pressures the receive task).
- **Risks/notes:** needs real NICs/PTP for production; CI uses loopback unicast only (no multicast, no PTP) — gate the integration test. No FFI, stays `unsafe_code = forbid`. The pgroup→NV12 unpack may need a real conversion path; if heavy, defer to a sub-task and assemble planes first.
- **Read first:** docs/research/streaming-gotchas.md §5 (master clock) + §7; ADR-0009 (data vs IO plane); `crates/multiview-input/src/st2022_7.rs` (reconstructor contract).

### `[~]` IN-3 — NDI ingest: runtime-loaded SDK → `FrameProducer` + CLI wiring · effort: XL · deps: none (but shares the FrameProducer→CLI bridge with IN-2)
- **Goal:** Replace the hard error at `crates/multiview-cli/src/pipeline.rs:2930` ("NDI ingest is not wired") with a real NDI receive source, runtime-loaded behind the `ndi` feature with operator license acceptance, feeding tiles like any other source.
- **Touches:** new `crates/multiview-input/src/ndi/` module (per ADR-0015 consequences: "no new crate"); `multiview-input/Cargo.toml` `ndi` feature (currently empty `"ndi" = []` — add `grafton-ndi` + `libloading`, both already in `Cargo.lock`); `crates/multiview-cli/src/pipeline.rs` `ingest_plan_for` (@2909) + `SourceLocation` enum (@3071, add `Ndi { name }`) + `ingest_loop`/`open_and_stream` (@3225/@3285, add an NDI branch that bypasses libav); `SourceKind::Ndi { name }` already exists (`schema.rs:266`).
- **Approach:**
  1. Implement the two-path model from ADR-0008/docs/io/ndi.md §2: `NDIlib_v6_load()` dynamic-load backend via `libloading` (default, no SDK to build) and an optional build-time `grafton-ndi` link. Probe at runtime → if unresolved, report capability absent (never crash).
  2. Add the **runtime license gate** (ADR-0008): NDI source refuses to start until `[system.ndi] accept_license` is set; surface a clear status, audited who/when. Wire this check in `ingest_plan_for` so an unaccepted NDI source produces a degraded tile, not a hung thread.
  3. Implement `NdiProducer: source::FrameProducer`: NDI `Receiver` → host-memory UYVY/P216/BGRA frame → `ProducedFrame` (host→GPU upload happens later in the compositor; this is the acknowledged copy boundary, ADR-0004). Wrap in NDI **FrameSync** per docs (per-source timing). Audio (FLTP) rebased like any source.
  4. CLI: change line 2930 from `Err(...)` to `Ok(IngestPlan{ location: SourceLocation::Ndi{name}, live: true, … })`; in `ingest_loop` route `SourceLocation::Ndi` to an NDI-specific drive loop (using the supervised-reconnect bracket already in `ingest_loop`) instead of `open_and_stream`.
- **Acceptance (done when):** with the runtime absent, `CapabilityReport`-style probe returns unavailable and an NDI source degrades its tile (LIVE→STALE→NO_SIGNAL) — assert via a unit test that `ingest_plan_for` no longer errors and that a missing-runtime NDI source never extends `PRIME_WAIT_BUDGET` (invariant #1, hard upper bound at pipeline.rs:~3083). A gated integration test (real runtime present, `#[ignore]` in CI) receives frames from a local NDI sender. `cargo deny check` on **default** features stays green (the proprietary deps must be fully behind `ndi`). Mandatory `ndi.video` attribution present in About/docs.
- **Risks/notes:** proprietary SDK — must never leak into the default build or `--all-features` deny job (ci.yml:69 runs default-only by design; verify `ndi` is excluded from any `full` preset). No real NDI network in CI → receive path is gated/ignored. `grafton-ndi` is `unsafe`/FFI — but it's an external crate; `multiview-input` stays `unsafe_code = forbid` (the FFI lives in grafton-ndi, not our code). License acceptance is load-bearing and audited.
- **Read first:** ADR-0008 (full); docs/io/ndi.md §2 (two code paths) + §3 (frame formats); ADR-0004 (copy boundary); ADR-M007 (CapabilityReport).

### `[x]` IN-4 — YouTube live: pure resolver core over `yt-dlp -J` · effort: M · deps: none
- **Goal:** Add a `youtube` module that spawns `yt-dlp`, parses its JSON info-dict, classifies `live_status`, and extracts the HLS `manifest_url` + `expire` deadline — pure and fixture-testable (ADR-0015 phase P0).
- **Touches:** new `crates/multiview-input/src/youtube/` module (ADR-0015: "no new crate"); `multiview-input/Cargo.toml` new off-by-default `youtube` feature (per docs: `youtube` requires `ffmpeg`; add `serde_json` already in workspace Cargo.toml:15); `crates/multiview-config/src/schema.rs` (new `SourceKind::Youtube { url }` — the enum is `#[non_exhaustive]` @209 so additive).
- **Approach:**
  1. Pure parse layer: `fn parse_info_dict(json: &str) -> Result<ResolvedHls>` reading `streamingData.hlsManifestUrl` equivalent from yt-dlp's `manifest_url`, `is_live`/`live_status`, and the `expire` Unix-timestamp query param. No network, no subprocess in the parse function (the spawn is a thin separate fn).
  2. Subprocess wrapper: spawn `yt-dlp -J --no-warnings <url>` via `tokio::process` with an argument vector (no shell), hard timeout, captured+redacted stderr (ADR-0015 security). Pin `--extractor-args "youtube:player_client=web_safari"`; avoid `ios`.
  3. `expire` parsing → a TTL/deadline type for the re-resolution loop (IN-5).
- **Acceptance (done when):** `crates/multiview-input/tests/youtube_resolve.rs` over **recorded yt-dlp JSON fixtures** (no network) asserts manifest extraction, `live_status` classification (live/upcoming/post-live-DVR), and `expire` parsing; a property test asserts no panic on malformed JSON. Capability probe (`yt-dlp --version`) returns unavailable cleanly when the binary is absent. No `unwrap`/`as`/indexing in non-test.
- **Risks/notes:** `yt-dlp` is runtime-discovered, never vendored (LGPL-clean by construction, Unlicense subprocess boundary). YouTube's player surface moves — ADR-0015 says cited yt-dlp line refs + ~6 h TTL must be re-verified at implementation time. Cookies are secret-ref only (ADR-M006).
- **Read first:** ADR-0015 (full) + docs/io/youtube-live.md §1 + §10 (phases P0–P1); ADR-M006 (secrets).

### `[~]` IN-5 — YouTube live: wire to HLS ingest + re-resolution loop · effort: L · deps: IN-4
- **Goal:** Feed the resolved googlevideo HLS URL into the existing `hls`/libav ingest path and run a control-plane re-resolution loop that refreshes before `expire`, so a YouTube tile survives the ~6 h manifest expiry (ADR-0015 phases P2–P4).
- **Touches:** `crates/multiview-cli/src/pipeline.rs` `ingest_plan_for` (@2909 — map `SourceKind::Youtube` to `SourceLocation::Url(resolved)`, `live: true`, reusing the exact rtsp/hls/ts/srt/rtmp branch @2927); the re-resolution task in `crates/multiview-input/src/youtube/`; reuse `reconnect.rs` backoff and the `PtsWallClock`/`open_and_stream` HLS path unchanged.
- **Approach:**
  1. At plan build, resolve once (IN-4) → `SourceLocation::Url`; a resolve failure must degrade the tile, never fail the build (the @2945 comment: "must never fail the *build* of a never-ending live source").
  2. Re-resolution loop on the control/IO plane (ADR-0009): parse `expire`, refresh at lead-time, do a make-before-break URL swap (ADR-R004/M005 Class-1 style), and re-resolve immediately on a sustained 403 burst. Run as a supervised subtask with hard timeout + bounded backoff (`reconnect.rs`); a hung `yt-dlp` is killed, not awaited (invariant #10).
  3. Surface staleness/extraction-failure alarm + resolver version via telemetry.
- **Acceptance (done when):** unit test asserts the swap is make-before-break (old URL stays live until new one primes) and that a resolve failure yields a degraded tile while the **output clock never stalls** (invariant #1) and never back-pressures (#10). A long soak test (gated/manual, real network) spans ≥1 expiry boundary with no tile loss. ffprobe the resolved `*.googlevideo.com` URL is a readable HLS master in the manual test.
- **Risks/notes:** real network + a current `yt-dlp` (ideally a JS runtime like Deno for n-sig) needed — gate the live test, keep CI on fixtures. Extraction breakage is *expected and handled*, not a hard failure.
- **Read first:** ADR-0015 consequences ("re-resolution is load-bearing"); ADR-T004 (HLS pacing) + docs/io/inputs.md §3 (input pacer); ADR-R003/R004.

### `[~]` IN-6 — WebRTC ingest: ICE/DTLS/SRTP transport behind an application-layer media engine · effort: XL · deps: IN-1 (shares the RTP→assembler→FrameProducer bridge)
- **Goal:** Turn the compile-only `webrtc::transport::WebRtcSession` shell into a real receive session, driving the pure SDP negotiation (`negotiate_answer`, mod.rs:266) through ICE/DTLS/SRTP into RTP → the IN-1 assembler → `IngestPump`.
- **Touches:** `crates/multiview-input/src/webrtc/transport.rs` (current shell holds only `NegotiatedSession`); `webrtc/Cargo.toml` feature (currently `"webrtc" = []`); a `WebRtcProducer: source::FrameProducer`. Per the module doc, the concrete media-engine binding "is supplied by the application layer" — so define the trait/seam here, keep the native WebRTC crate out of `multiview-input` to preserve the pure/LGPL-clean default.
- **Approach:**
  1. Define a `MediaEngine` trait in `transport.rs` (the application-layer seam): `start(negotiated) -> rtp packet stream`. `multiview-input` provides the SDP negotiation + the RTP→frame adapter; the ICE/DTLS/SRTP engine (e.g. `str0m`, sans-IO, pure-Rust — verify license before adopting) is wired at the application layer behind the `webrtc` feature so the default build stays pure.
  2. `WebRtcSession::start` consumes `NegotiatedSession` (already held), drives the engine, and routes decrypted RTP payloads (matching the negotiated `Codec`/payload type from `mod.rs`) into IN-1's assembler for video (H264/VP8 keyframe-gated) and the audio rebaser.
  3. Bridge to `FrameProducer` exactly as IN-2 (bounded channel, drop-oldest, never block — invariant #10). RTP timestamp → `raw_pts`, 32-bit wrap.
- **Acceptance (done when):** `crates/multiview-input/tests/webrtc_sdp.rs` stays green (pure negotiation unchanged); a new gated integration test (`#[ignore]` in CI, needs a peer) negotiates an answer and receives RTP through a fake/loopback `MediaEngine`, asserting frames reach the `TileStore`. `cargo check --features webrtc` + `cargo clippy --features webrtc -- -D warnings` green. Re-assert #1/#10 (sampled, never pacing).
- **Risks/notes:** no ICE peer/TURN/real network in CI → engine path is gated/ignored; the sans-IO engine choice must be license-vetted (LGPL-clean default) and kept at the application layer, not pulled into `multiview-input` (which stays `unsafe_code = forbid`, no native WebRTC lib). Highest-effort item; consider phasing (negotiation seam + adapter first, full engine second).
- **Read first:** the `webrtc::transport` module doc (compile-only rationale) + `webrtc/mod.rs` `negotiate_answer`; docs/research/streaming-gotchas.md §7 (RTCP SR lip-sync, jitter); ADR-0009.

### `[x]` IN-7 — CI strategy: feature-gated compile + integration gating for the wired transports · effort: S · deps: IN-2, IN-3, IN-6
- **Goal:** Ensure the four new gated paths are clippy/check-verified without requiring NICs/peers/SDKs in CI, closing the gap that today's clippy job (ci.yml:32) runs **default features only** and never lints `st2110`/`webrtc`/`ndi`/`youtube`.
- **Touches:** `.github/workflows/ci.yml` (the `fmt + clippy` job @21 and `check + test` job @34).
- **Approach:**
  1. Add a `cargo check`/`cargo clippy -- -D warnings` matrix over `--features st2110`, `--features webrtc`, `--features youtube` (all NIC/network-free to *compile*). Keep `ndi` compile-checked via the dynamic-load backend only (no SDK fetch); the build-time `grafton-ndi` link stays in a separate Docker/CI lane that fetches the SDK (ADR-0008 consequences).
  2. Mark every real-hardware test `#[ignore]` (or `#[cfg_attr(not(feature=…), ignore)]`) so `cargo test --workspace` stays green on shared runners; document a manual/self-hosted lane for loopback ST 2110, an NDI sender, and a YouTube/WebRTC live soak.
  3. Keep `cargo deny check` on **default** features only (ci.yml:69) so the proprietary `ndi` deps never reach the license allowlist.
- **Acceptance (done when):** CI compiles+clippies all four features green with zero native deps for the pure paths; `cargo test --workspace` excludes hardware tests; `cargo deny check` still passes default-only. No new `-D warnings` violations introduced by the gated code.
- **Risks/notes:** the `ndi` build-time-link lane needs the proprietary SDK fetched in Docker (ADR-0008) — keep it off the default LGPL-clean matrix. Don't enable `--all-features` in any deny/license job (would pull `gpl-codecs` + proprietary `ndi` by design).
- **Read first:** ci.yml (current jobs); ADR-0008 consequences (CI/Docker SDK fetch); ADR-0012 (LGPL-clean default).

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-input/src/source.rs
- /workspaces/mosaic/crates/multiview-input/src/st2110/transport.rs
- /workspaces/mosaic/crates/multiview-input/src/webrtc/transport.rs
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs
- /workspaces/mosaic/crates/multiview-config/src/schema.rs


## CTL — Control plane → engine

Grounding summary (verified by reading the scaffold): the engine's per-tick control hook is `command_drain` at `crates/multiview-cli/src/control.rs:140`, invoked from `EngineRuntime::run_*_with_control` at `crates/multiview-engine/src/runtime.rs:420` (line 420, before `compose`). The drain currently handles only `Command::SwapSource`; all other accepted commands fall into the `_ => false` arm (`control.rs:150`). HTTP submit is non-blocking via `submit_accepted` → `CommandSender::try_submit` (`routes/mod.rs:181`, `command.rs:242`). The shared `EnginePublisher<EngineStateSnapshot, Event>` is `Clone` and `publish_event` is a non-blocking drop-oldest broadcast (`isolation.rs:347`) — so the drain can emit outcome events directly without any new engine channel and without violating inv #10. Sources/Outputs/Overlays CRUD persist opaque `serde_json::Value` bodies to `ResourceRepository` (`resource_store.rs`, `routes/sources.rs`) but never touch the engine. `MultiviewConfig.{sources,cells,overlays,outputs}` are typed (`multiview-config/src/lib.rs:91–100`). `CompositorDrive` exposes `set_layout` and `insert_store` (`drive.rs:127,136`); there is no canvas/output reconfig (Class-2 territory per ADR-R004).

The central design decision threaded through every item: **give the drain closure a cloned `Arc<EnginePublisher<…, Event>>`** so commands apply *and* emit their outcome on the realtime stream from the one place that already runs at the frame boundary, all non-blocking.

---

### `[~]` CTL-1 — Drain-apply every accepted command + emit outcome events  · effort: L · deps: none
- **Goal:** Make the engine actually apply Start/Stop/ApplyLayout/Arm/Take/CancelSalvo/SetTallyOverride at the frame boundary (today they 202 then no-op), and emit each one's outcome on the event stream, so the WebUI's accepted commands take effect and are observable.
- **Touches:** `crates/multiview-cli/src/control.rs:140` (`command_drain`), `crates/multiview-cli/src/main.rs:206,312` and `:328` (drain construction + `run_until_stopped_with_control` call), `crates/multiview-cli/src/run.rs:393` (signature already threads `publisher`), `crates/multiview-engine/src/isolation.rs:347` (`publish_event`), `crates/multiview-events/src/event.rs` (`SalvoEvent`/`SalvoPhase`/`OutputStatus`/`OutputRunState`).
- **Approach:**
  1. Change `command_drain`'s signature to also take `publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>` (clone of the one already built at `main.rs:179`/`:281`); store it in the returned closure. This is read-share of a `Clone` publisher whose `publish_event` is non-blocking — no new channel, no engine change.
  2. Introduce an internal program-running flag (`bool`) captured in the closure. `Command::Start`/`Stop` flip it; emit an `Event::OutputStatus { state: Running/Stopped, .. }` (`event.rs:107`) correlated to `op`. Wire the realtime layer to set `Envelope::corr` from the op id (see CTL-5 — this item just emits; CTL-5 carries `corr` to the wire).
  3. For `Command::ApplyLayout { layout, .. }`: resolve the named layout from the working config/resource store and `drive.set_layout(Arc::new(solved))` (reuse the existing `solve_layout` + `set_layout` path already proven for `SwapSource` at `control.rs:156`). Emit success/failure outcome.
  4. For salvo commands (`ArmSalvo`/`TakeSalvo`/`CancelSalvo`): a salvo is a named layout+binding recall. Arm stages a solved layout in the closure's state; Take calls `set_layout`; Cancel discards. Emit `Event::SalvoArmed/Taken/Cancelled(SalvoEvent::new(salvo, phase).with_head(head))` (`event.rs:250`). (Salvo body resolution detail shared with CTL-5; full multi-head semantics can be a follow-up — land arm/take/cancel of the salvo's layout here.)
  5. `SetTallyOverride` is the engine's tally arbiter's concern; if the arbiter isn't wired into the software engine yet, keep it accepted and emit a `TallyState` echo, leaving arbitration to the tally stream. Do **not** silently no-op without an event.
  6. Keep every arm panic-free: no `unwrap`/indexing; an unknown layout/salvo logs `tracing::warn!` and emits a failure outcome (mirroring the existing `solve_layout` error handling at `control.rs:164`).
- **Acceptance (done when):** New unit tests in `control.rs` `mod tests`: `start_then_stop_emits_output_status`, `apply_layout_swaps_active_layout` (assert `drive.layout()` id changed), `unknown_layout_emits_failure_not_panic`, `salvo_take_applies_armed_layout`. A drain-loop test asserts that submitting N commands and draining is O(pending) and never awaits. Invariant to re-assert: **#1 output-clock never stalls** and **#10 no engine back-pressure** — add/extend a soak test driving `run_for_with_control` while flooding the bus, asserting `RunReport.faltered == false` and `frames == ticks`. ffprobe is not applicable (no new output here); content check is the composited layout actually changing post-ApplyLayout.
- **Risks/notes:** The drain must stay allocation-light on the hot loop (runtime.rs:413 comment is explicit). `publish_event` is drop-oldest, so an outcome event *can* be dropped under a slow consumer — that is correct per inv #10; never block to guarantee delivery. LGPL-clean (pure Rust, no codec/NDI). No hardware/network needed in CI. Guardrail: no `unwrap`/`as`/indexing in the closure.
- **Read first:** ADR-W013 (path #2 + #3, slice A3), `docs/research/management-capability-matrix.md` §1.3, ADR-W008 referenced for the 202+corr contract.

### `[x]` CTL-4 — `ApplyLayout` HTTP route  · effort: S · deps: CTL-1
- **Goal:** Add the missing `POST /api/v1/commands/apply-layout` (or `POST /api/v1/program:take`) route so the WebUI can request a layout change — the command enum variant and the drain handler exist, but no HTTP entry point does.
- **Touches:** `crates/multiview-control/src/routes/mod.rs` (alongside `cmd_start`/`cmd_stop`/`cmd_swap` at `:220–265`), the router registration in `crates/multiview-control/src/router/` (where `/commands/*` are mounted), `crates/multiview-control/src/openapi.rs` (path registration).
- **Approach:**
  1. Add a `ApplyLayoutRequest { layout: String }` body type (mirror `SwapRequest` at `routes/mod.rs:244`).
  2. Add `async fn cmd_apply_layout` following `cmd_swap` exactly: `require(Action::Write)`, optional `authorize_object`, then `submit_accepted(&state, &idem, |op| Command::ApplyLayout { op, layout })` (`routes/mod.rs:181`), then `state.audit(.., AuditAction::Command, "layout", &layout, ..)`.
  3. Register the route in the same table as `/commands/start`; add the `#[utoipa::path]` annotation + OpenAPI registration so `/docs` shows it.
- **Acceptance (done when):** New route test (mirroring existing command-route tests): `apply_layout_returns_202_with_op_id`, `apply_layout_requires_write_role` (403 for viewer), `apply_layout_sheds_503_when_bus_full`. Re-assert inv #10: the handler only `try_submit`s (never blocks). OpenAPI doc test asserts the path appears in `/api/v1/openapi.json`.
- **Risks/notes:** Idempotency-Key handling is free via `submit_accepted` (note the release-on-shed logic at `routes/mod.rs:206`). No licensing/hardware concerns. Guardrail: reuse `submit_accepted`; do not hand-roll a second submit path.

### `[~]` CTL-3 — Mirror `multiview run` config into the resource store at startup + on change  · effort: M · deps: none
- **Goal:** Seed the Sources/Outputs/Overlays (and layouts) resource stores from the loaded `MultiviewConfig` when `bind_and_serve` starts, so those WebUI pages are non-empty under a live run, and keep them in sync as commands mutate the working config.
- **Touches:** `crates/multiview-cli/src/control.rs:78` (`AppState::new` construction in `bind_and_serve`), `crates/multiview-control/src/state.rs:135` (`AppState`/`with_*_store` builders at `:240–257`), `crates/multiview-control/src/resource_store.rs:94` (`ResourceInput` body), `crates/multiview-config/src/lib.rs:91–100` (typed `sources`/`cells`/`overlays`/`outputs`).
- **Approach:**
  1. In `bind_and_serve`, accept the `&MultiviewConfig` (the caller at `main.rs:192`/`:298` already has it). Before constructing `AppState`, build in-memory stores and `create(&id, ResourceInput { body })` one resource per `config.sources` / `config.outputs` / `config.overlays`, with `body = serde_json::to_value(source)?` (each config type is serde-typed). Seed layouts/cells into the layout repository similarly.
  2. Pass the seeded stores into `AppState` via the existing `with_sources_store`/`with_outputs_store`/`with_overlays_store` builders (`state.rs:240–257`) instead of the default empty `InMemory*Store`.
  3. For "on change": when CTL-2 applies a CRUD change to the engine, write the same body back so the store stays authoritative; conversely when CTL-1 applies a `SwapSource`/`ApplyLayout`, update the mirrored cell/layout body. Keep the resource store the single source of truth that the drain reads from.
- **Acceptance (done when):** Integration test: build a config with 3 sources + 2 outputs + 1 overlay, call `bind_and_serve`, then `GET /api/v1/sources` returns 3 id-sorted resources (currently empty). `mirror_roundtrips_source_body` asserts `serde_json::from_value::<Source>(body)` equals the config source. Re-assert inv #10: seeding happens once at bind, off the engine hot loop. No ffprobe applicable.
- **Risks/notes:** Body is opaque JSON by design (`resource_store.rs` doc + `sources.rs:9`), so engine-side validation stays at apply time — don't tighten the store schema. Watch for id collisions (config validation already enforces unique source ids at `config/src/lib.rs:409`). LGPL-clean. Guardrail: `serde_json::to_value` returns `Result` — handle, don't `unwrap`.

### `[ ]` CTL-2 — Apply Source/Output/Overlay CRUD to the running engine via the command bus  · effort: L · deps: CTL-1, CTL-3
- **Goal:** Make a successful `POST/PUT/DELETE` on `/api/v1/sources|outputs|overlays` actually reconfigure the running engine (today it only persists to SQLite), so editing a source/overlay in the WebUI changes the live composite.
- **Touches:** `crates/multiview-control/src/routes/sources.rs:74–135` (create/update/delete), `routes/outputs.rs`, `routes/overlays.rs`, `crates/multiview-control/src/command.rs:81` (the `#[non_exhaustive]` `Command` enum — add variants), `crates/multiview-cli/src/control.rs:140` (drain handler), `crates/multiview-engine/src/drive.rs:136` (`insert_store`).
- **Approach:**
  1. Extend `Command` with `UpsertSource { op, id, body }` / `RemoveSource { op, id }` and overlay equivalents (enum is already `#[non_exhaustive]`, and `kind()`/`operation_id()` at `command.rs:154,169` just need new arms). Outputs that change pinned params are Class-2 (CTL-6); a hot output edit (e.g. bitrate) is a separate non-layout command.
  2. In each mutating handler (after the store write + audit, e.g. `sources.rs:82`), `submit_accepted`-style `try_submit` the corresponding command so the change reaches the drain. Keep the store write authoritative; the bus submit is best-effort (shed→503 only if you choose to gate; for CRUD prefer: persist always, submit non-blocking, surface bus-full as a warning, not a failed write).
  3. In `command_drain`, handle the new variants: deserialize `body` into the typed `Source`/`Overlay`, rebuild the working `MultiviewConfig`, re-solve, and `set_layout` (overlay/cell changes) or build+`insert_store` a new `TileStore` (source add/replace) — `insert_store` already exists at `drive.rs:136`. Emit an outcome event.
  4. Classify each per the capability matrix: most source/overlay edits are **Class-1 (Hot)** (matrix §2.1, §2.6); surface the class via the existing plan/dry-run surface if present, else default Hot.
- **Acceptance (done when):** Tests: `update_source_reaches_engine` (after PUT, drain applies and `drive` reflects the new store/binding), `delete_overlay_removes_layer`, `crud_write_persists_even_when_bus_full` (store write succeeds, command shed logged). Re-assert **#10** (handlers never block; drain non-blocking) and **#1** (soak: continuous CRUD churn, `faltered == false`). Content check: composited frame changes after a solid-source color edit (compare `y_plane`, as `run.rs` tests do at `:704`).
- **Risks/notes:** Deserializing the opaque body can fail — the drain must log+emit-failure, never panic (no `unwrap`). A source add allocates a `TileStore` + synthetic frame; keep that off the tick (build it in the handler or a sibling task, hand the ready `Arc<TileStore>` to the drain) to honour the hot-loop allocation rule. LGPL-clean for synthetic kinds; real-decoder sources need the `ffmpeg` pipeline path (`main.rs:173`), not the software engine — gate accordingly. Guardrail: bound the body size; no indexing.

### `[x]` CTL-5 — Salvo/Start/Stop outcome events on the realtime stream (corr-correlated)  · effort: M · deps: CTL-1
- **Goal:** Carry each command's outcome to the WebUI on the realtime stream correlated by its operation id, so a 202'd Start/Stop/Salvo shows its eventual result (the ADR-W008 contract: result arrives on the stream, not the HTTP body).
- **Touches:** `crates/multiview-control/src/realtime.rs:158` (`next_delta` — sets the envelope), `:204` (`event_scope_id`), `:213` (`topic_for_event` — already maps `SalvoArmed/Taken/Cancelled`→`Tally`, `OutputStatus`→`Outputs`), `crates/multiview-events/src/envelope.rs:102` (`with_corr`), and CTL-1's emission site.
- **Approach:**
  1. Thread the `OperationId` from the command into the emitted event so the drain can stamp `corr`. Simplest grounded path: carry the op id alongside the event from the drain (e.g. emit events that already know their op) and set `Envelope::with_corr(op)` in `next_delta` — extend `event_scope_id`-style with an `event_corr_id(&event)` helper, OR (cleaner) have the drain publish via a small wrapper that records corr. Pick the helper approach to avoid changing the `Event` enum shape.
  2. Confirm `topic_for_event` already routes the salvo/output events correctly (it does — `realtime.rs:218,228`); just ensure `corr` is populated on those frames.
  3. Verify the SSE path (`sse_handler`, `realtime.rs:385`) and WS path (`run_ws_session`, `:342`) both carry `corr` (they share `SessionStream::next_delta`, so one change covers both).
- **Acceptance (done when):** Tests: `salvo_take_outcome_carries_corr` (drive a take, assert the streamed `Envelope.corr == op`), `output_status_event_on_topic_outputs`. A `SessionStream`-level unit test asserting lagged-skip still holds (re-assert inv #10: a slow client resubscribes, `realtime.rs:191`, never back-pressures). No ffprobe.
- **Risks/notes:** `corr` is `Option<String>` (`envelope.rs:71`) so non-command events stay `corr: None`. Outcome events ride the same drop-oldest broadcast — a lagged client may miss one and re-baseline; that's acceptable and tested. LGPL-clean. Guardrail: no `unwrap` in the projection.

### `[ ]` CTL-6 — Class-2 parallel-output (make-before-break) migration  · effort: XL · deps: CTL-1, CTL-2, CTL-4, CTL-5
- **Goal:** Implement controlled-reset migration for pinned-param changes (codec, geometry-beyond-max, pixel format, GOP structure, canvas fps/resolution) as a new parallel output spun up and cut over while the original keeps running — the only correct way to change pinned params without a downstream-visible falter (ADR-R004).
- **Touches:** new logic in `crates/multiview-cli/src/control.rs` (drain) + `crates/multiview-cli/src/pipeline.rs` (the `ffmpeg` output path — `main.rs:173`), `crates/multiview-output/*` (output session lifecycle), `crates/multiview-control/src/command.rs` (a `MigrateOutput { op, id, new_config, cutover }` command per matrix §2.3 `POST .../migrate`), `routes/outputs.rs` (the migrate route + `POST .../plan` dry-run surfacing `reset_required`), `crates/multiview-events/src/event.rs` (`OutputRunState::Migrating` already exists at `:100`).
- **Approach:**
  1. Add the `plan`/dry-run surface first: a handler that classifies a proposed output change as Class-1/reset-lite/Class-2 per the pinned-param list in ADR-R004 and matrix §1.3, returning `reset_required` **before** apply (this is the inv #11 contract). Pure function over old-vs-new config; no engine change.
  2. Add `Command::MigrateOutput`; in the drain (or a sibling task it signals — migration is heavyweight and must NOT run on the tick loop), stand up a second output session with the new pinned config while the original keeps emitting (`OutputRunState::Migrating`), then atomically cut consumers over and tear down the original (make-before-break, ADR-R004 decision).
  3. Because building/tearing an encoder session is expensive and may block, do it on a **sibling task**, not in `command_drain` — the drain only flips the active-output pointer at a frame boundary once the new session reports ready. This preserves inv #1/#10 (the tick loop never builds an encoder).
  4. Emit `OutputStatus { state: Migrating → Running }` outcomes (CTL-5 carries corr).
- **Acceptance (done when):** Tests: `plan_classifies_codec_change_as_class2`, `plan_classifies_bitrate_change_as_hot`, and a migration integration test asserting the original output never gaps during cutover (the load-bearing claim). ffprobe check: on the **real** output (`ffmpeg` feature), probe both old and new streams across the cutover and assert continuous PTS / no `EXT-X-DISCONTINUITY` on the surviving consumer until cutover, new SPS/PPS only on the new session. Re-assert **#1** (output-clock never stalls during migration) and **#10** (encoder build is off-loop). 
- **Risks/notes:** This is the largest item and the only one needing the `ffmpeg` pipeline + real encoder; software-engine has no output session to migrate, so the end-to-end ffprobe test must run under `--features ffmpeg` (+`gpl-codecs` for software H.264/H.265) — keep the default LGPL-clean build green by feature-gating the migration encoder path and testing the *classifier* (pure Rust) unconditionally. CI without an encoder can run the plan/classifier tests; the cutover ffprobe test needs an `ffmpeg`-featured runner. Guardrail: no encoder construction on the hot loop; no `unwrap` anywhere.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/control.rs
- /workspaces/mosaic/crates/multiview-control/src/command.rs
- /workspaces/mosaic/crates/multiview-control/src/routes/mod.rs
- /workspaces/mosaic/crates/multiview-control/src/realtime.rs
- /workspaces/mosaic/crates/multiview-cli/src/main.rs


## PRV — Preview & WebRTC transport

Grounded against `docs/research/preview-subsystem.md`, ADR-P001..P005, ADR-0006, and the current scaffold: `crates/multiview-preview/src/{whep.rs,tap.rs,encode.rs,framing.rs,token.rs}`, the engine isolation primitive (`crates/multiview-engine/src/isolation.rs:141` `EventStream`/`EventSubscription`, drop-oldest broadcast), the output fan-out (`crates/multiview-output/src/fanout.rs:105` `PacketSink`, `:117` `PacketRouter`), the LL-HLS segmenter (`crates/multiview-output/src/hls/media.rs`), the control seam (`crates/multiview-control/src/preview.rs:21` `PreviewProvider`, `routes/preview.rs`), and the HAL ladder (`crates/multiview-hal/src/degradation.rs:49`).

Key facts that shape the plan: WHEP today is **pure SDP/codec negotiation only** (`whep.rs:111` `WhepSession::negotiate`); there is **no native dep** in the workspace (no str0m/webrtc-rs/gstreamer) and the `webrtc` feature pulls none by design. The `PreviewProvider` trait in control is JPEG-only — there is no WHEP/SDP route wired in control yet. The degradation ladder (`degradation.rs:49`) has **no preview rung** despite ADR-P001/P005 mandating preview be shed first.

---

### `[~]` PRV-1 — Native ICE/DTLS/SRTP transport behind a `WhepTransport` seam (str0m, in-process default)  · effort: XL · deps: none
- **Goal:** Turn the WHEP SDP scaffold into a working sub-250ms focus media path by adding a transport seam and a native (str0m) implementation, so `negotiate()` produces an answer that actually carries SRTP video — without coupling preview to the engine.
- **Touches:** `crates/multiview-preview/src/whep.rs` (extend `WhepSession`, `whep.rs:91`), new `crates/multiview-preview/src/whep/transport.rs`; `crates/multiview-preview/Cargo.toml` (new optional `str0m` dep gated under a *new* `webrtc-native` feature, kept separate from the existing pure `webrtc` feature so the negotiation-only build stays dep-free per `lib.rs:38`); `Cargo.toml` workspace `[workspace.dependencies]`; `deny.toml` (license/advisory entries for str0m + ring/openssl chain).
- **Approach:**
  1. Define a `trait WhepTransport { fn accept(&self, offer:&str, codec:PreviewCodec, media: PreviewMediaSource) -> Result<TransportAnswer>; fn close(&self, session_id); }` in `transport.rs`. `WhepSession::negotiate` keeps doing pure codec selection (`select_codec`, `whep.rs:199`); the transport fills ICE ufrag/pwd, DTLS fingerprint, bundle/mid into the answer that `build_answer_sdp` (`whep.rs:209`) currently leaves as `0.0.0.0` placeholders — refactor `build_answer_sdp` to accept transport-supplied attributes rather than hard-coding.
  2. Add `Str0mTransport` under `#[cfg(feature = "webrtc-native")]`: str0m is sans-IO (pure Rust, no C, no openssl — preferred for LGPL-clean default and the lean-binary goal), drive its `Rtc` state machine on a dedicated tokio task in **Tier A** (per ADR-P001), feeding it RTP from the preview encoder (PRV depends on the encoder pool — wire to `PreviewMediaSource` which pulls from a `TapLease`, `tap.rs:202`).
  3. The media source reads NV12 from a `TapLease::recv` (`tap.rs:224`) → preview H.264 encode → packetize → str0m `write`. Encoder selection already returns `PreviewCodec` (`whep.rs:127`); H.264 baseline preferred.
  4. Keep MediaMTX sidecar as an *alternative* `WhepTransport` impl stub (republish path) per ADR-0006 "Sidecar reuse" — document it as the v1 fallback terminator; do not build it fully here, just leave the seam.
- **Acceptance (done when):** New `tests/whep_transport.rs` (TDD): (a) `negotiate` + `Str0mTransport::accept` yields an answer with non-placeholder ICE ufrag/pwd + DTLS fingerprint + `a=candidate`; (b) a loopback integration test (feature-gated, behind `#[ignore]` unless a runtime env flag is set, since UDP/STUN in CI is unreliable) establishes DTLS-SRTP and decodes ≥1 H.264 NAL via a str0m client. ffprobe check: pipe the egress to ffprobe and assert `codec_name=h264` + resolution = the preview thumbnail size. Re-assert **inv #10**: the media source only holds a `TapLease` (drop-oldest); add an assertion that the transport task holds no `EventStream` publish handle and never awaits the engine.
- **Risks/notes:** Prefer str0m to keep the default build LGPL-clean and C-free; webrtc-rs pulls a heavier stack. Gate strictly behind `webrtc-native` (off by default) so `cargo deny`/the pure build stay green (`lib.rs:43` `forbid(unsafe_code)` must hold — str0m is safe Rust). UDP/STUN/TURN availability in CI is unreliable → loopback-only, env-gated. No `unwrap`/`as`/indexing in non-test (RRTP seq/timestamp arithmetic must use checked/`wrapping_*`).
- **Read first:** preview-subsystem.md §4 (transport table) + §8 "Sidecar reuse"; ADR-P002; ADR-0006.

### `[x]` PRV-2 — Wire WHEP focus routes into `multiview-control` (POST/DELETE per scope) with token-gated Focus + transport seam  · effort: L · deps: PRV-1
- **Goal:** Expose the WHEP focus endpoints the brief §5 specifies so the SPA can actually open a focus session, enforcing `AccessScope::Focus` and the focus cap at the HTTP edge.
- **Touches:** `crates/multiview-control/src/routes/preview.rs` (add handlers alongside `program_jpeg`, `routes/preview.rs:35`), `crates/multiview-control/src/routes/mod.rs` (route registration), `crates/multiview-control/src/preview.rs` (extend `PreviewProvider` seam, `preview.rs:21`, with a `whep_negotiate`/`whep_close` capability OR a sibling `WhepProvider` trait so control stays codec-free), `crates/multiview-control/src/state.rs` (hold the new provider in `AppState`).
- **Approach:**
  1. Add `POST /api/v1/preview/program/whep`, `POST /api/v1/preview/inputs/{id}/whep`, `POST /api/v1/preview/outputs/{id}/whep` (+ matching `DELETE …/{session_id}`) per brief §5. Each: SDP offer body in (`Content-Type: application/sdp`), `201 Created` + `Location:` resource URL + answer SDP body out; `503` with `application/problem+json` (RFC 9457 per control CLAUDE.md) + `fallback: ws-jpeg|llhls` hint when cap hit or HW budget unavailable. *(Historical as-shipped record — the hint literal is renamed to `jpeg` in the webrtc-full push, ADR-P006/W020.)*
  2. Verify `AccessScope::Focus` via the existing `TokenIssuer::verify` (`token.rs:270`) — control must map its `Principal`/`Action` (`routes/preview.rs:13`) to a minted preview token scoped to the exact `TapKey`. Reuse the `WhepError::AccessDenied` → `403` mapping.
  3. Delegate the actual negotiation+transport to a `WhepProvider` the binary implements (same isolation discipline as `PreviewProvider`); control never links str0m.
- **Acceptance (done when):** `tests/preview_whep_routes.rs` in control (TDD): View token → `403`; malformed SDP → `400`; valid Focus offer → `201` + `Location` + `application/sdp` answer; `DELETE` of the resource URL → `204` and frees the session (assert subscriber count via descriptor drops to 0). OpenAPI (`openapi.rs`) regenerates with the new paths. Inv #10: assert the handler path holds only a `TapLease`/`WhepProvider` handle, never the engine.
- **Risks/notes:** Auth/SSRF: cue/whep schemes must stay allowlisted (ADR-P004) — not in scope here but the route must reject non-allowlisted ids. Keep control free of native deps (the seam is a trait object). RFC 9457 problem+json for all error bodies. No `unwrap` in handlers.
- **Read first:** preview-subsystem.md §5 (API tables) + §2; ADR-P002; control CLAUDE.md API conventions.

### `[x]` PRV-3 — Concurrent-focus session caps + isolation enforcement (the `FocusGate`)  · effort: M · deps: PRV-2
- **Goal:** Bound worst-case preview load deterministically — hard caps on concurrent WHEP focus sessions (per-operator and server-wide) with "open second focus demotes the first," and admit focus encode sessions only from leftover budget after program reserves first.
- **Touches:** new `crates/multiview-preview/src/focus.rs` (a `FocusGate` admission gate); `crates/multiview-preview/src/lib.rs` (re-export, `lib.rs:57`); integrate with `crates/multiview-hal/src/cost.rs` (`CostBudget`, `cost.rs:24`) and `crates/multiview-hal/src/degradation.rs` for shed-first; `crates/multiview-preview/src/token.rs` (the `AccessScope::Focus` cap is "only enforceable when Focus is granted explicitly," `token.rs:106` — the gate is where that promise is kept).
- **Approach:**
  1. `FocusGate` holds an atomic/Mutex map of active focus sessions keyed by operator + a server-wide counter, both with configured caps (default e.g. 1/operator per ADR-P002, N server-wide). `try_acquire(operator, key) -> Result<FocusLease, AdmissionDenied>`: enforce per-operator cap by **demoting** (closing) the operator's prior focus lease, and the server-wide cap by returning `AdmissionDenied { fallback: WsJpeg|LlHls }` (drives PRV-2's `503` hint).
  2. Admission against HAL: before granting, check a preview-encode session is available from leftover budget (program sessions reserved first per ADR-P001 §8). Reuse `CostBudget` (`cost.rs:24`); do **not** invent a parallel resource model.
  3. `FocusLease` Drop releases the slot (mirrors `TapLease` Drop, `tap.rs:245`), so a dropped/timed-out session frees the cap.
- **Acceptance (done when):** `tests/focus.rs` (TDD): (a) opening a 2nd focus for one operator returns a lease and the 1st lease is closed/demoted; (b) server-wide cap+1 returns `AdmissionDenied` carrying a `fallback`; (c) dropping all leases returns active count to 0 (idle-cost invariant, ADR-P003); (d) a property test that the live focus count never exceeds the cap under concurrent acquire/release. Inv #10: the gate uses only a short-lived `Mutex` the engine never touches (same pattern as `TapRegistry`, `tap.rs:26`).
- **Risks/notes:** Base Apple silicon = 1 encode engine (brief §4) → cap WHEP to 1 and prefer JPEG; make caps config-driven, never hard-coded (probe via HAL). No `unwrap`/indexing; saturating arithmetic on counters.
- **Read first:** preview-subsystem.md §3 ("CAP CONCURRENCY") + §8 ("Shared budgets"); ADR-P003; ADR-P002.

### `[x]` PRV-4 — Make preview the topmost (cheapest-to-shed) degradation rung  · effort: M · deps: PRV-3
- **Goal:** Honor the non-negotiable ADR-P001 guarantee that preview is shed *before any program lever moves*, by extending the HAL ladder so preview suspension precedes rung 0.
- **Touches:** `crates/multiview-hal/src/degradation.rs` (`DegradationAction` enum `:27`, `LADDER` `:49`, `rung` `:62`, `affects_program` `:81`); `crates/multiview-preview/src/focus.rs` (a `suspend()`/`resume()` driven by the ladder); the binary glue that observes `Hysteresis` (`degradation.rs:202`).
- **Approach:**
  1. Prepend new cheapest rungs to the ladder *above* `DropTileResolution`: `ShedFocusWhep` → `DropPreviewGridFps` → `DropPreviewGridRes` → `DropOffAirCueDecoders` → `SuspendPreviewEntirely`, matching brief §8's "topmost rung" list. Update `LADDER`, `rung()`, `MAX_LEVEL` (`:87`), and crucially `affects_program()` (`:81`) so the program-affecting boundary shifts to *after* all preview rungs.
  2. Wire the planner's `Hysteresis::observe` (`degradation.rs:251`) consumer to call `FocusGate::suspend()` (PRV-3) on the preview rungs, returning `503`/`fallback` to in-flight focus opens. Log every preview adaptation like any other shed action (operator trust, ADR-P003).
- **Acceptance (done when):** `degradation.rs` tests extended (TDD): assert preview rungs occupy levels below the first program rung, `affects_program()` is `false` for all preview rungs and `true` only at the first tile/output lever, and a test that climbing pressure sheds *all* preview rungs before any `DropTile*`/`LowerOutput*`. The PRV-1/PRV-2 no-back-pressure chaos test must still pass with preview suspended. Inv #9 (cheapest-impact-first) + inv #10 re-asserted.
- **Risks/notes:** This changes a load-bearing enum in the protected-core HAL — coordinate with the engine/HAL work-stream; the change is additive (prepending rungs) but `MAX_LEVEL` shifts, so any persisted level mapping must be migrated. No behavior change to existing rungs' relative order.
- **Read first:** preview-subsystem.md §3 + §8 ("First on the degradation ladder"); ADR-P001; multiview-engine CLAUDE.md inv #9/#10.

### `[~]` PRV-5 — Sub-second WebRTC OUTPUT (program) focus: program-canvas tap → preview encode → WHEP  · effort: L · deps: PRV-1, PRV-2, PRV-3
- **Goal:** Deliver sub-second program (and per-output) focus over WebRTC — the "OUTPUT/PROGRAM focus" rows of brief §4 — built on the same transport, gate, and tap seams as input focus, with mandatory real-vs-approx labeling for outputs.
- **Touches:** `crates/multiview-preview/src/tap.rs` (a program-canvas `TapKey` of `TapScope::Program` and per-output `TapScope::Output`, `token.rs:35`); `crates/multiview-output/src/fanout.rs` (register a *separate* preview `PacketSink` on the existing `PacketRouter::register`, `fanout.rs:133`, depth-1-3 drop-oldest per ADR-P001 — never the encoder readback ring); the preview encode source in `whep/transport.rs` (PRV-1); a `preview/source` descriptor (`routes/preview.rs`) reporting `REAL ENCODED OUTPUT (tap:<proto>)` vs `PRE-ENCODE CANVAS APPROX` per ADR-P005.
- **Approach:**
  1. PROGRAM focus: append a GPU downscale blit into a dedicated preview ring (own `EventStream`, `isolation.rs:188`), skipped entirely when subscriber count is 0 (ADR-P003 conditional tap). The `TapLease` feeds the PRV-1 H.264 preview encode → str0m egress. Label always `PRE-ENCODE CANVAS APPROX`.
  2. OUTPUT focus: register a preview `PacketSink` (`fanout.rs:105`) on the target `RenditionId` that O(1)-clones `Arc<EncodedPacket>` into a depth-1-3 drop-oldest ring, decode-back at reduced res (`skip_frame=nokey`), re-encode small, egress. Label `REAL ENCODED OUTPUT (tap:<protocol>)`. For HLS-family outputs, prefer redirect to the published playlist (zero extra encode, ADR-P005) instead of a WHEP encode.
  3. Reuse PRV-3 `FocusGate` for caps and PRV-4 shed-first.
- **Acceptance (done when):** `tests/program_output_whep.rs`: PROGRAM focus with 0 subscribers performs **no blit** (assert the conditional-skip), first subscriber starts it, last-leave + linger stops it. OUTPUT focus registers exactly one extra `PacketSink` and the `route()` count (`fanout.rs:168`) increases by 1 (encode-once preserved — no second encode of the canvas). ffprobe the OUTPUT WHEP egress and assert its color/resolution matches the *tapped rendition*, not the pre-encode canvas. `GET …/preview/source` returns the correct label. Chaos: SIGKILL/stall the program-focus consumer and assert byte-for-byte-unchanged program output + zero added frame-interval jitter (ADR-P001 hard gate, inv #1 + #10).
- **Risks/notes:** Output decode-back adds decode-engine load (worst on Intel/AMD/CPU, ADR-P005) → reduced-res + I-frames-only + capped; HLS outputs take the zero-decode published-playlist path. The program downscale blit needs GPU; in CI without a GPU, gate the blit test on a software/headless path or `#[ignore]`. Never share the encoder NV12 readback ring (ADR-P001 — explicit audit). gpl-codecs/ndi stay opt-in; NDI output preview is a convention-tagged host-frame tap, not a VUI-tagged bitstream (ADR-P005).
- **Read first:** preview-subsystem.md §1 (three scopes) + §2 (isolation table) + §7 (mermaid taps); ADR-P005; ADR-P001.

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-preview/src/whep.rs
- /workspaces/mosaic/crates/multiview-preview/src/tap.rs
- /workspaces/mosaic/crates/multiview-engine/src/isolation.rs
- /workspaces/mosaic/crates/multiview-output/src/fanout.rs
- /workspaces/mosaic/crates/multiview-control/src/routes/preview.rs


## ENG — Engine timing & resilience


### `[x]` ENG-1 — Bounded teardown join for a wedged sink (task #50) · effort: M · deps: none
- **Goal:** Make `drive_streaming` always return on stop even when an output sink thread is wedged in a blocking muxer/network write, so no infinite hang on teardown — without touching inv #1 (the engine already emits all N ticks past a wedged sink).
- **Touches:** `crates/multiview-cli/src/pipeline.rs` — `StreamEgress::join` (`pipeline.rs:1629`), the teardown sequence at `pipeline.rs:1088` (`egress.join()`), and the existing precedent `IngestSupervisor::join_all` (`pipeline.rs:2269`) + `INGEST_JOIN_GRACE` (`pipeline.rs:3128`). New test alongside `crates/multiview-cli/tests/streaming_encode.rs`.
- **Approach:**
  1. Mirror `IngestSupervisor::join_all`'s bounded-detach loop into `StreamEgress::join`: replace the unconditional `self.consumer.join()` and per-sink `handle.join()` with a `JoinHandle::is_finished()` poll loop against a deadline `Instant::now() + EGRESS_JOIN_GRACE` (add the const next to `INGEST_JOIN_GRACE`, value ~2 s as in ADR-0026 §5 "mirroring `INGEST_JOIN_GRACE`").
  2. Join the consumer first (it drops sink senders on exit, unblocking each sink's `rx.recv()` with `Err` — the exact mechanism the existing `streaming_encode` blocked-runner test relies on, lines 117-123). A sink still unfinished after the grace window is **detached + logged** (`tracing::warn!`), and `join` returns a partial `EgressOutcome` rather than blocking. Detach is safe: sinks own their own libav muxer state freed in `Drop`, reaped at process exit (same justification as the ingest detach comment, `pipeline.rs:2264`).
  3. Preserve the finalize-on-error contract: a sink that *does* finish within grace still has its trailer written / playlist flushed (`pipeline.rs:1640`).
- **Acceptance (done when):**
  - New `#[tokio::test]` `wedged_sink_teardown_returns_within_grace` (peer of `live_blocked_sink_stays_bounded_and_never_stalls`): a runner that pulls one frame then blocks on a recv that *never* sees disconnect (e.g. holds its own clone of the sender, or `loop { park }`) — assert `drive_streaming_for_test` returns `Ok` and wall-time-to-return < grace + margin; assert a detach was logged.
  - Re-assert **inv #1** unchanged: `result.report.frames == TICKS` still holds (the engine loop already returned before teardown). Re-assert **inv #10**: `peak_occupancy <= capacity + 1` unchanged.
  - Existing `streaming_encode.rs` tests stay green (the well-behaved detach-on-disconnect path must not regress to a premature detach).
- **Risks/notes:** No new deps, no unsafe, no licensing change. Guardrail: the detach path must not `unwrap`/panic on a poisoned handle — match and log. Risk: detaching a sink mid-trailer yields a truncated artifact; that is the correct trade (a wedged sink can't produce a valid trailer anyway) and must be reported, not hidden (ADR-0025 honest-falter). This is the standalone slice of ADR-0026 §5; land it independently of the encoder hoist.
- **Read first:** ADR-0026 §5 (bounded teardown folds in #50); resilience-and-av §2.2 ("hard timeout + process kill — a worker wedged in FFI never observes the token").

---

### `[~]` ENG-2 — Input PTS normalizer + pacer reroute (ADR-0021 points 1-3) · effort: XL · deps: none
> **As-built (ENG-2, red `dab5a9b` → green `8c581a7`):** the inv-#3 correctness core is shipped. `multiview_ffmpeg::DecodedVideoFrame` now carries `raw_pts` (the raw best-effort source-tick PTS, pre-rescale, threaded through every constructor); CLI `open_and_stream` builds a per-input `PtsNormalizer::new(WrapBits::Mpeg33, stream_time_base, declared_fps)` and a new `timeline_pts(&mut normalizer, raw_pts, fallback)` seam routes the raw PTS through it (delta-unwrap → genpts fallback → discontinuity re-anchor → strict-monotonic guard, anchored at `master_now = 0` for the source-relative origin) before the existing `PtsWallClock` pace/publish. Deterministic cli unit tests (`eng2_timeline_tests`, sleep-free): monotonic-across-33-bit-wrap and genpts-advances; the normalizer's broader matrix is unit-proven in `multiview-input/tests/normalize.rs`. **Deliberately not done (→ ENG-2b):** `PtsWallClock` was NOT replaced by `multiview-input::Pacer` (it already satisfies inv #4 wall-clock-by-PTS and now consumes the *normalized* monotonic timeline — the swap is cosmetic), and `ReorderBuffer`/`jitter` was NOT wired (the libav demux→decode path already presents frames in presentation order; packet-level reorder is a raw-RTP/ST-2110 ingest concern). Content-aware wrap-boundary end-to-end stays on the GPU/soak tier.
- **Goal:** Wire the existing-but-unused `multiview-input::PtsNormalizer` + `Pacer` + `ReorderBuffer` into the file/VOD + live ingest loop so wrap/discontinuity/jitter classes are handled (latch-on-tick already fixed the race/freeze; this adds the *additional* class ADR-0021 §"Not built" tracks), retiring the ad-hoc `PtsWallClock`.
- **Touches:** `crates/multiview-cli/src/pipeline.rs` — `ingest_loop` / `open_and_stream` path and the `PtsWallClock` read-ahead throttle; `crates/multiview-input/src/normalize.rs` (`PtsNormalizer`), `src/pacer.rs` (`Pacer`), `src/jitter.rs` (`ReorderBuffer`) — confirm these exist and their unit tests; `crates/multiview-ffmpeg::StreamVideoDecoder` (currently keeps the cadence-derived genpts via `with_declared_fps` per the As-built note — decide whether genpts moves to `PtsNormalizer` or stays).
- **Approach:**
  1. First read `crates/multiview-input/src/normalize.rs` + `pacer.rs` + `jitter.rs` and their tests to confirm the exact `normalize(raw, master_now_ns)` / `submit(pts, now_ns)` signatures (ADR-0021 §Consequences says they are pure functions returning emitted `MediaTime` / `Release` decisions).
  2. Route each decoded frame's `best_effort_timestamp`-or-`pts` (+ the stream `r_frame_rate`) through `PtsNormalizer` (33-bit/32-bit delta-unwrap into i64, genpts from declared cadence, discontinuity re-anchor on `EXT-X-DISCONTINUITY`/TS indicator/`|jump|>~10 s`, first-frame anchor, strict-monotonic guard) before stamping the `TileStore` publish, replacing the source-relative `pts − first_pts` stamping the As-built fix shipped.
  3. Keep the latch-on-tick sampling as the *primary* correctness mechanism (do **not** regress `framestore::read_at`); the pacer/normalizer is the ingest-gate refinement. Decide genpts ownership: ADR-0021 §18 wants it in `PtsNormalizer` (cadence from per-input `r_frame_rate`), but the As-built note says the decoder's `with_declared_fps` already meets cadence-correctness — pick one to avoid double genpts and document it.
- **Acceptance (done when):**
  - The ADR-0021 §Consequences "adversarial matrix" is covered by deterministic tests (injected clock, no sleeping): CFR 24/25/30, VFR, B-frames/non-monotonic-received, no-PTS, mpegts ~1.44 s offset, mid-stream discontinuity, PTS gap, **33-bit/32-bit wrap boundary**, off-output-fps 24/25/29.97/30/50/60. The decisive guard from §Consequences: *no-PTS-at-25 + 24 fps output emits a smooth measured-cadence schedule* (must fail the old 29.97 constant).
  - **Content-aware end-to-end** (not part of the flake-free unit gate, but the verification owner): overlays-OFF `tblend=difference`→`signalstats` YAVG vs a ground-truth ffmpeg encode across a synthetic wrap boundary — render real-time, rendered motion ≈ ground-truth (the §53 correction mandates content-aware, never overlay-laden).
  - Re-assert **inv #1**: the pacer gates *ingest only*; the output `out_pts = f(tick)` is untouched (no test may show output cadence changing with a bursting/wrapping source). Re-assert **inv #10**: ingest feeds the lock-free store, no back-pressure to the engine.
- **Risks/notes:** Largest item; the wrap-boundary class is exactly the "ran fine for an hour, fails overnight" trap (ADR-T003 §Consequences) — synthetic-timestamp tests are mandatory, real soak is separate (≥72 h zero-gap, ADR-0021 §"Soak/GPU tier"). No new deps, no unsafe. Guardrail: all time stays i64 ns / exact rationals (inv #3); no `unwrap`/`as` on the unwrap arithmetic — use checked/`i128` intermediates as `PtpServo` does.
- **Read first:** ADR-0021 (esp. §As-built + §"Not built"), ADR-T003 (the unwrap/genpts/monotonic decision), streaming-gotchas §0.

---

### `[ ]` ENG-2b — Pacer/ReorderBuffer decision + content-aware wrap-boundary soak (ENG-2 follow-on) · effort: M · deps: ENG-2
- **Goal:** Close the remaining ENG-2 acceptance: (a) decide & document whether `PtsWallClock` stays or is replaced by `multiview-input::Pacer`, and whether `ReorderBuffer`/`jitter` is wired at all on the libav path; (b) add the content-aware end-to-end wrap-boundary verification on the GPU/soak tier.
- **Touches:** `crates/multiview-cli/src/pipeline.rs` (`PtsWallClock` vs `Pacer`); `crates/multiview-input/src/pacer.rs`, `src/jitter.rs` (confirm they are needed only for raw-RTP/ST-2110 ingest, where the demuxer does not reorder); a GPU-tier soak/golden harness.
- **Approach:**
  1. Document the as-built decision: the libav `receive_frame` path yields presentation-ordered frames, so `ReorderBuffer` belongs to the future raw-RTP/ST-2110 reader, **not** this path; `PtsWallClock` already implements inv #4 (wall-clock-by-PTS) on the now-normalized timeline, so the `Pacer` swap is pure code-reuse — do it only if it removes the duplicate. Land the decision in ADR-0021 §As-built.
  2. Content-aware wrap-boundary: overlays-OFF `tblend=difference`→`signalstats` YAVG vs a ground-truth ffmpeg encode across a synthetic 33-bit wrap (real-time render). GPU/soak tier, gated — never a flake-prone unit test.
- **Acceptance (done when):** the Pacer/ReorderBuffer decision is documented in ADR-0021 (or wired with tests if kept); a gated content-aware wrap-boundary test exists (run on the soak tier, honestly noted if the tier is unavailable in CI).
- **Risks/notes:** Mostly a documentation + verification-tier item now that the inv-#3 correctness core (ENG-2) is wired and unit-proven; no new hot-path code expected on the libav demux path.
- **Read first:** ADR-0021 §As-built, ENG-2's As-built note above.

---

### `[x]` ENG-3 — NTP/PTP lock auto-detect for the wall-clock badge (task #37) · effort: M · deps: ENG-5 (shares the syscall binding)
- **Goal:** Replace the *assumed* `RefStatus::Locked` on the on-screen clock badge with a *measured* kernel lock-state (Linux `adjtimex`, macOS `ntp_adjtime`), so the overlay clock honestly shows Locked / Holdover / Freerun (`RefStatus` already has all three — `crates/multiview-overlay/src/clock.rs:281`).
- **Touches:** `crates/multiview-cli/src/wallclock.rs` — `SystemWallClock::reference()` (`wallclock.rs:118`) currently returns the static `status` field; `RefStatus` enum (`multiview-overlay/src/clock.rs:281`, `.is_locked()` at :322 covers Locked|Holdover). The binding cannot live in `multiview-cli` (`#![forbid(unsafe_code)]`, `wallclock.rs:33`).
- **Approach:**
  1. Add a tiny, sampled lock-status reader behind the same syscall binding ENG-5 introduces (a `nix`-style safe wrapper, or a `deny(unsafe_code)` FFI shim sub-crate). On Linux call `adjtimex`/`clock_adjtime` and map: `STA_UNSYNC` set ⇒ `Freerun`; synced + within tolerance ⇒ `Locked`; synced but flagged stale/holdover ⇒ `Holdover`. macOS `ntp_adjtime` analog.
  2. Make `SystemWallClock` sample this **at draw time** through the existing injectable `WallClock::reference()` seam — keep it a pure read, off the hot path, never pacing (the module doc's anti-drift contract, `wallclock.rs:39-45`).
  3. Default-safe: if the syscall is unavailable (container without the cap, unknown platform), fall back to the current assumed status rather than panicking — the `FakeClock` test seam (`wallclock.rs:182`) already proves injectability.
- **Acceptance (done when):**
  - New tests asserting the mapping (injected raw `adjtimex` result → expected `RefStatus`), including the `STA_UNSYNC`→`Freerun` and unavailable→fallback arms; the existing `system_clock_default_reports_sys_locked` test must be updated to reflect measured status (or kept for the fallback path).
  - The badge renders text+glyph for all three states (a11y: never colour alone — `clock.rs:275`), verified by the overlay render test.
  - Re-assert **inv #1**: lock status is *sampled at draw*, never pacing; advancing the injected clock still advances displayed time-of-day independent of lock state.
- **Risks/notes:** CI has no real PTP/NTP grandmaster — the syscall returns a value (usually `Freerun`/`STA_UNSYNC` in a container), so the *real* read is exercised but the *Locked* assertion must use the injected mapping, not a live grandmaster. Unsafe is confined to ENG-5's binding. Licensing: `nix`/`rustix` are MIT — LGPL-clean.
- **Read first:** `crates/multiview-cli/src/wallclock.rs` module docs (§"Reference status — honest about what we can detect"); ADR-T003/T001 for the timing-reference posture.

---

### `[x]` ENG-4 — Linux i915/amdgpu GPU load probe · effort: L · deps: none
- **Goal:** Implement the real `SysfsLoadProbe` (currently returns `LoadSample::Unavailable { reason: "Linux sysfs/i915 PMU load probe not yet implemented" }`, `load.rs:630`) so the scheduler gets live AMD/Intel `DeviceLoad` snapshots — sampled off the hot path at 1-4 Hz, never pacing (ADR-0017).
- **Touches:** `crates/multiview-hal/src/load.rs` — `linux_sysfs` module (`load.rs:600-633`), gated behind `vaapi`/`qsv`; mirrors the `NvmlLoadProbe` reference impl (`load.rs:491+`). Telemetry gauges in `multiview-telemetry` (ADR-0017 §Consequences: register only known metrics).
- **Approach:**
  1. `devices()`: enumerate `/sys/class/drm/card*/device/` render nodes, read `vendor`/`device` PCI ids to classify AMD vs Intel and build a stable `DeviceId` from the **PCI bus id** (not the enumeration index — `load.rs:61` identity rule).
  2. AMD `sample()`: read `gpu_busy_percent` (sysfs) → `gpu_busy_frac`; `mem_info_vram_used`/`mem_info_vram_total` → VRAM bytes; treat VCN4+ enc/dec as the **merged "Media engine"** figure (ADR-0017 §Rationale) — populate `enc_util_frac`/`dec_util_frac` only if a per-engine source exists, else leave `None` (honest unknown, never fabricated zero, `load.rs:140`).
  3. Intel `sample()`: MVP via DRM **fdinfo** per-engine `drm-engine-*` counters (plain file reads, **no unsafe**); defer the i915 **PMU** `perf_event_open` path (that one needs `unsafe`) to a follow-up — sysfs/fdinfo is the safe-first landing the scaffold comment already promises.
  4. All reads are plain `std::fs::read_to_string` + parse with checked conversions; any missing/garbled file ⇒ `LoadSample::Unavailable` (graceful, `load.rs:628`), never a panic.
- **Acceptance (done when):**
  - Unit tests over **fixture sysfs/fdinfo text** (golden files, injected path root) asserting parse → `DeviceLoad` fields, including the malformed-file → `Unavailable` arm and the VCN4 merged-media arm. The `vram_frac` clamp tests (`load.rs:678`) already model the over-total guard — reuse.
  - The `select` policy tests still pass with a blind-field probe (ADR-0017 §Consequences: "a blind-vendor probe falls back to VRAM + overall-busy without blocking placement").
  - Re-assert **inv #1 (chaos gate)**: "the probe can never stall the engine" — a hung/slow sysfs read inside `LoadPoller::poll` (called on the engine's dedicated blocking thread, `load.rs:395`) must be bounded; selection happens at admission/reconfig only, never per-frame.
- **Risks/notes:** CI has **no AMD/Intel GPU** — real-hardware sampling is unverifiable here, so tests run against captured fixtures (the same posture as NVML's graceful-init); a hardware soak is a separate tier. The fdinfo walk is a §5-risk-7 cost on tiny boxes — keep it inside the clamped `PollInterval` (1-4 Hz, `load.rs:438`). No unsafe for the sysfs/fdinfo MVP; PMU deferred. Licensing: pure `std::fs`, no new deps — LGPL-clean.
- **Read first:** ADR-0017 §Decision + §Rationale (vendor-asymmetric metric matrix); gpu-monitoring-and-scheduling §1-§2.5.

---

### `[x]` ENG-5 — PTP / ST 2059 PHC NIC binding (`ptp` feature) · effort: L · deps: none (blocks ENG-3's syscall shim)
- **Goal:** Bind the disciplined-reference servo (`PtpServo`, fully tested, `ptp.rs`) to a real PTP Hardware Clock: read the host PHC and feed `(offset, delay)` samples into the servo, behind the off-by-default `ptp` feature — sampled, **never pacing** the output clock.
- **Touches:** `crates/multiview-engine/src/ptp.rs` — the `phc` module (`ptp.rs:263-306`), currently compile-only wrapping `DisciplinedReference`. Feature `"ptp" = []` (`engine/Cargo.toml:44`). Engine forbids unsafe (`lib.rs:87`), so the raw `clock_gettime(dynamic_clock_from_fd)`/`clock_adjtime` calls need a binding boundary.
- **Approach:**
  1. Add `nix` (or `rustix`) as an **optional dep pulled in only by `ptp`** to get safe wrappers for opening `/dev/ptpN`, `clock_gettime` on the dynamic clock id (`FD_TO_CLOCKID`), and `PTP_SYS_OFFSET*` ioctls — avoiding a local `unsafe` override in the engine. If a needed ioctl isn't wrapped, isolate it in a thin `deny(unsafe_code)` + `// SAFETY:` sub-module gated by `ptp` (the workspace lint posture, `Cargo.toml:43`).
  2. Implement a `PhcReader` that, per sample tick, reads PHC-vs-system offset and path delay and constructs `PtpSample::new(offset_ns, delay_ns)` (the existing constructor clamps negative delay, `ptp.rs:58`), feeding `DisciplinedReference::apply` (`ptp.rs:290`).
  3. Run the reader on a dedicated sampled thread (like `LoadPoller`), publishing the servo's `offset_ns`/`frequency_ppb` into a wait-free `LatestState`/`ArcSwapOption` snapshot (`isolation.rs:56`) — the engine *reads* the estimate, never gates a tick on it.
- **Acceptance (done when):**
  - `cargo build -p multiview-engine --features ptp` and `clippy` clean (compile-verified — this env has no PTP NIC, as the module doc states `ptp.rs:18-19`).
  - The servo math stays under its existing `ptp_servo.rs` property tests (unchanged); add a `PhcReader` test with an **injected fake PHC source** (offset/delay it controls) proving samples flow to the servo and the published snapshot tracks `servo.offset_ns()`.
  - Re-assert **inv #1 — the load-bearing one for this stream**: a test proves the output `OutputClock` tick count is identical with the PTP reader producing wild/absent samples vs. with it off; the servo disciplines only the *separate reference estimate* (`ptp.rs:21-35`). PHC is **sampled, never pacing**.
- **Risks/notes:** CI/devcontainer has **no PTP-capable NIC** — runtime-verified only via the injected fake; real verification is a hardware tier. `ptp` is opt-in (default LGPL-clean build never links it). The `unsafe` boundary must be minimal and `// SAFETY:`-documented (engine is `forbid(unsafe_code)` — the override is local to the gated module only). Guardrail: integer ns / ppb only (inv #3), no float fps.
- **Read first:** `ptp.rs` module docs (§"Invariant #1 is preserved"); ADR-T003; core-engine §11 ("no genlock/PTP over arbitrary RTSP/HLS — PTP only for ST 2110 uncompressed-over-IP").

---

### `[x]` ENG-6 — HA cluster peer transport (`cluster` feature) · effort: L · deps: none
- **Goal:** Implement a real `ClusterTransport` (peer heartbeat sockets + replication snapshot/delta wire I/O) behind the off-by-default `cluster` feature, driving the already-tested `HaRunner`/`HaStateMachine`/`ReplicaApplier` — best-effort, drop-oldest, never back-pressuring the active engine.
- **Touches:** `crates/multiview-engine/src/ha/transport.rs` — the `ClusterTransport` trait (`transport.rs:35`) and `HaRunner` (`transport.rs:90`) are done and pure; only a concrete socket impl is missing. `repl.rs` `EngineSnapshot`/`ReplicationDelta` are already serde-tagged (`repl.rs:117`). Feature `"cluster" = []` (`engine/Cargo.toml:52`).
- **Approach:**
  1. Implement a `UdpClusterTransport` (or a small TCP variant) satisfying `ClusterTransport`: `publish_heartbeat` serialises a `Heartbeat` and does a non-blocking `send` (drop on `WouldBlock` — the trait contract is "must never block or back-pressure", `transport.rs:36-43`); `poll_heartbeat` is a non-blocking `recv` returning `None` when empty (`transport.rs:48`).
  2. `publish_snapshot`/`publish_delta` serialise via the existing serde models (`repl.rs:73,117`) — JSON for cross-node wire per the repl doc (`repl.rs:71`); `poll_replication` deserialises into `ReplicationMessage` (`transport.rs:74`). Bound the inbound queue (drop-oldest) so a flooding peer can't grow memory.
  3. Wire under an off-hot HA thread that calls `HaRunner::pump_heartbeats`/`pump_replication`/`beat` (`transport.rs:130,142,156`) on the injected `MediaTime` — the runner already makes every *decision* via the pure model; the socket layer only moves bytes.
- **Acceptance (done when):**
  - `cargo build -p multiview-engine --features cluster` + clippy clean (compile-verified — no live multi-node cluster here, `transport.rs:1-8`).
  - A **loopback two-instance test** (two transports on localhost): node B promotes when node A stops beating past the miss deadline, and a `LayoutSwap` delta replicates A→B and applies contiguously (reusing `ha_failover.rs`/`ha_replication.rs` model tests over the real transport). A dropped delta surfaces `ApplyError::VersionGap` and triggers a snapshot re-request (`repl.rs:285`), proving no silent divergence.
  - Re-assert **inv #1 + #10**: a partitioned/flapping/slow link changes only *when a standby decides to promote*, never the active's `out_pts = f(tick)` or its send path — a chaos test with a black-holed socket shows the active's tick count unaffected (`mod.rs:23-44`).
- **Risks/notes:** Loopback is testable in CI; true multi-host failover (split-brain across a real partition) is a hardware/network tier. `cluster` is opt-in (default build never links it). Use `std`/`tokio` UDP — no GPL deps (LGPL-clean). Guardrail: no `unwrap` on socket/serde results; a malformed datagram is dropped + logged, never a panic; sends are non-blocking so the publisher never stalls (inv #10).
- **Read first:** `crates/multiview-engine/src/ha/mod.rs` + `transport.rs` + `repl.rs` module docs (the isolation contract); resilience-and-av §2 (supervision, no-split-brain), §1 (output guarantee).

---

### Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs
- /workspaces/mosaic/crates/multiview-engine/src/ptp.rs
- /workspaces/mosaic/crates/multiview-engine/src/ha/transport.rs
- /workspaces/mosaic/crates/multiview-hal/src/load.rs
- /workspaces/mosaic/crates/multiview-cli/src/wallclock.rs


## GPU — Compositor, efficiency & hardware


### `[x]` GPU-1 — Hoist the single encoder into the bake consumer; fan packets to mux-only sinks · effort: L · deps: none
- **Goal:** Make inv #7 actually hold across outputs: a file + HLS run at one rendition must encode the canvas once, not twice, by moving the lone `VideoEncoder` into `consumer_main` and fanning `EncodedPacket`s to `PacketMuxSink`s.
- **Touches:** `crates/multiview-cli/src/pipeline.rs` — `consumer_main` (1682), `StreamEgress::spawn` (1568), `SinkRunner` type (1448), `run_one_output` (1729), `RunnableOutput` enum (~328), `StreamingFrameSource` (~1735). Uses the existing `multiview_output::PacketMuxSink`/`PacketSource` (`sink.rs:720`) and `multiview_ffmpeg::{EncodedPacket, StreamCodecParameters, VideoEncoder}`. The `EncodeConfig` already lives on the `Pipeline` (`pipeline.rs:565`).
- **Approach:**
  1. In `consumer_main`, after `StreamBaker::new`, build **one** `VideoEncoder` from the pipeline's resolved `EncodeConfig.target()` (the same config `FileSink`/`SegmentSink` consume today), and snapshot `StreamCodecParameters::from_encoder` + `encoder.time_base()` once.
  2. Change the per-sink fan-out channel element from `Arc<Nv12Image>` to `EncodedPacket` (the `sync_channel` in `spawn`, `SINK_QUEUE_CAP` unchanged — packets are ≪ NV12 so the bound is still cheap). Per baked frame: run the existing `FrameConverter` NV12→`yuv420p` + tick-PTS re-stamp (lift the converter logic out of `sink.rs:230` into the consumer, or reuse it), `encoder.send_frame`, drain `receive_packet`, wrap each in `EncodedPacket`, and **`clone()` once per live sink** (`EncodedPacket::Clone` is `av_packet_ref`, `packet.rs:108`) before the blocking `tx.send`.
  3. Replace each `SinkRunner` body: `run_one_output` builds a `PacketMuxSink::file`/`::segment`/`::push` and drives `PacketMuxSink::run(&mut source, &codec_params, time_base)` over a new `PacketStreamSource` (the `EncodedPacket`-fed twin of `StreamingFrameSource`, draining the `Receiver<EncodedPacket>`). The `StreamCodecParameters` + `time_base` are cloned into each runner closure at spawn (both are `Send`, independent copies — `packet.rs:170`).
  4. On encoder-build failure inside the consumer, return `PipelineError::Engine` so the egress join surfaces it (the existing `drop(sink_txs)` finalizes partial sinks).
  5. Fold #50 bounded teardown: the existing `egress.join()` already joins consumer-first then sinks; add a grace-deadline join for a wedged `PacketMuxSink` (mirror the ingest `INGEST_JOIN_GRACE` constant) so `drive_streaming` always returns, detaching + logging a still-unfinished sink.
- **Acceptance (done when):** TDD: (a) a unit/integration test on the cli streaming seam (`drive_streaming_for_test`, `pipeline.rs:789`) with **two** outputs at one rendition asserts exactly **one** `VideoEncoder` is constructed — add a `#[cfg(test)]` `ENCODER_BUILDS` counter mirroring `SEED_ENCODER_BUILDS` (`sink.rs:61`); (b) an `ffprobe` content check that the file output and the HLS segments carry the **same** coded stream (identical packet count / keyframe positions); (c) re-assert inv #1 (output clock never stalls — the existing `faltered=false` / `frames==ticks` check) and inv #10 (the hot loop still only `try_send`s; a slow `PacketMuxSink` paces the consumer, never the engine — extend the existing slow-sink test). Invariant #7 verified by the encoder-count == 1.
- **Risks/notes:** LGPL-clean — the single encoder still selects via `resolve_encoder` (`pipeline.rs:299`), GPL `x264`/`x265` stay behind `gpl-codecs`; nothing new linked. No hardware/network needed (mpeg2video default, CI has ffmpeg). Guardrails: the per-sink clone must be `EncodedPacket::clone` then `into_owned_packet` inside the sink (so `write_packet`'s in-place rescale is sound — already the `PacketMuxSink` contract); no `unwrap`/`as`/indexing in the consumer loop. Watch the per-GOP seed wart: the segment path already builds its seed **once** (the `SEED_ENCODER_BUILDS==1` test, `sink.rs:1289`) — the packet-fed `PacketMuxSink::segment` seeds each segment muxer from the one `StreamCodecParameters` snapshot (`sink.rs:802`), so the wart is structurally gone; assert it stays gone.
- **Read first:** ADR-0026 (the exact 5-step decision), efficiency §2.3, ADR-E004; `multiview-output/CLAUDE.md`.

---

### `[x]` GPU-2 — Converge the SOFTWARE engine onto `synth::generator_loop` so a clock source animates · effort: M · deps: none
- **Goal:** A clock source in an overlay-built **software** run animates (one bake/sec) instead of showing a static placeholder card, by spawning the existing `generator_loop` per animated source rather than priming a single static frame.
- **Touches:** `crates/multiview-cli/src/run.rs` — `SoftwareEngine` (145), `build` (185), `prime_stores` (462), `software_source_frame` (610), the `run_*` entry points (289/342/376). Reuses `crate::synth::{SyntheticKind, generator_loop}` (`synth.rs`) and `multiview_framestore::TileStore` (already the store type, `run.rs:151`).
- **Approach:**
  1. In `build`, classify each source via `SyntheticKind::from_source_kind` (`synth.rs:72`). For a **static** kind (`Bars`/`Solid`, `animated()==false`) keep the current prime-once path. For an **animated** kind (`Clock`), record it for a generator thread instead of baking a placeholder.
  2. Add a generator-supervisor to the software run mirroring the ffmpeg pipeline's pattern (`pipeline.rs:3229`): for each animated source spawn a thread running `generator_loop(kind, &store, w, h, canvas_color, cadence, &stop)`; own a shared `Arc<AtomicBool> stop` and join all generators on run teardown (the `sleep_until` chunked stop, `synth.rs:348`, makes teardown prompt).
  3. Thread the existing `StopSignal` from `run_until_stopped*` into that `AtomicBool` (or bridge it) so Ctrl-C tears generators down; for the bounded `run_for`/`run_for_realtime` paths, raise the stop after the tick budget.
  4. Replace the `_ =>` placeholder arm in `software_source_frame` for animated kinds with the honest fallback only when `overlay` is off (the generator returns `OverlayRequired`, `synth.rs:260`) — keep the per-index card for genuinely undecodable kinds (rtsp/hls/etc.), preserving the `a_decoded_kind_does_not_masquerade_as_bars` test (`run.rs:725`).
- **Acceptance (done when):** TDD: a software-run test under `feature = "overlay"` with a `clock` source asserts the tile's `TileStore` content **changes across a second boundary** (content-aware: sample the program canvas at displayed-second N vs N+1, assert `y_plane` differs — the same shape as `analog_clock_renders_and_animates`, `synth.rs:464`). Re-assert inv #1: the existing `RunReport.faltered == false` / `frames == ticks` (`run.rs:555`) still holds with generators running. A non-overlay build still produces a valid slate per tick (inv #1/#2) and the static `bars`/`solid` tests (`run.rs:699/711`) are unchanged.
- **Risks/notes:** No hardware. `clock` rendering needs `feature = "overlay"`; gate the generator spawn behind it and keep the GPU-free CPU bake (`apply_overlays_to_nv12`, `synth.rs:247`). Guardrails: generator threads must never block the engine (they publish into the lock-free `TileStore` the engine only samples — inv #10) and a render failure holds last-good (`synth.rs:332`), never panics. Don't double-publish: an animated source must not also be primed by `prime_stores`.
- **Read first:** core-engine §13 (output-core/isolation), ADR-0027 (synthetic sources), efficiency §2.6 (fps harmonization / re-render only on change).

---

### `[x]` GPU-3 — GPU `describe_*` metadata trait methods: wire or remove · effort: S · deps: none
- **Goal:** Resolve the dead `NotImplemented` `describe_output` on `GpuCompositor` (and the analogous `Decoder::describe_next`/`Encoder::describe_input` defaults) — either return real `FrameMeta` or remove the override so the scaffold is honest.
- **Touches:** `crates/multiview-compositor/src/gpu/compositor.rs:525` (`GpuCompositor::describe_output`), `crates/multiview-core/src/traits.rs:98-149` (the three trait defaults + `FrameMeta`). No real decoder/encoder currently implements the core `Decoder`/`Encoder` traits (the ffmpeg crate has its own concrete types), so those two are default-only.
- **Approach:**
  1. Decide per the brief: the `GpuCompositor` already knows its output geometry/color at construction — `describe_output` should return a real `FrameMeta` (NV12 canonical pixel format, inv #5; the canvas color tag, inv #8) instead of `Err(NotImplemented)`. Wire it: store the output `FrameMeta` on the compositor (or derive from the per-`composite` canvas spec) and return `Ok(meta)`.
  2. For `Decoder::describe_next` / `Encoder::describe_input`: confirm via grep that nothing calls them (no impls beyond the default). If genuinely unused, **remove the methods** from the traits rather than ship a permanent `NotImplemented` (cleaner core surface per `multiview-core/CLAUDE.md`); if a caller is planned, leave them but document the contract.
  3. Keep `multiview-core` FFI-free (its CLAUDE.md rule): `FrameMeta` construction stays pure.
- **Acceptance (done when):** TDD under `feature = "wgpu"`: a unit test builds a `GpuCompositor` and asserts `describe_output()` returns `Ok` with the expected width/height, `PixelFormat::Nv12`, and the canvas color tag (no `NotImplemented`). If the Decoder/Encoder defaults are removed: `cargo check --workspace` is green and no caller breaks. Re-assert inv #5 (the described format is NV12).
- **Risks/notes:** `wgpu` is off by default / no CI adapter — the `describe_output` test must be pure metadata (no dispatch), so it runs GPU-free like the existing `validate_overlay_shader` tests. Guardrails: no `unwrap`/`as`; build `FrameMeta` via its constructors.
- **Read first:** core-engine §3–§5 (trait surface), color-management (the output tag axes for `FrameMeta`).

---

### `[~]` GPU-4 — Overlay IMAGE-primitive GPU texture upload (the wgpu shader branch) · effort: L · deps: GPU-3
- **Goal:** Make the `KIND_IMAGE` branch in `overlay.wgsl` a real premultiplied-RGBA blit (DVB-sub / bitmap caption burn-in on GPU) instead of the transparent no-op, matching the CPU reference `blend_image` within SSIM/PSNR thresholds.
- **Touches:** `crates/multiview-compositor/src/gpu/shaders/overlay.wgsl:142` (the `KIND_IMAGE` no-op), `crates/multiview-compositor/src/overlay/gpu_subpass.rs` — `OverlayPrimGpu::pack` Image arm (181), `bind_group_layout` (350), `OverlaySubpass` (271); the CPU reference `blend_image` (`overlay/subpass.rs:701`) is the ground truth. Likely needs the GPU compositor to actually **dispatch** the overlay subpass (currently `gpu/compositor.rs` only runs composite+encode) and a new image-atlas/texture-array binding.
- **Approach:**
  1. Add an image-texture binding to the overlay bind-group layout (`gpu_subpass.rs:350`) — an `Rgba8Unorm` texture (or texture-array, layer per Image primitive) holding the premultiplied bitmap(s), mirroring the existing R8 glyph-atlas binding 2. Pack the layer index + atlas offset into the Image primitive's `kind_meta` (today zeroed, `gpu_subpass.rs:181`).
  2. In `overlay.wgsl`, replace the `coverage = 0.0` no-op (line 145) with a `textureLoad` of the image texel at `(dest + col,row)`, contributing its premultiplied RGBA into the same `over` accumulator (lines 155-159) — the bitmap is already premultiplied, so feed it as `src` directly (no `* coverage`), matching `blend_image`.
  3. Wire the upload: a `write_texture` of each Image cue's premultiplied bytes before dispatch (cap by `MAX_OVERLAY_PRIMS` / a bounded image-atlas size, no per-frame unbounded allocation — ADR-E005 bounded-memory rule the file already cites at line 26).
  4. Ensure the GPU compositor dispatches `OverlaySubpass` between composite and encode (the file header's stated position) — alias the overlay output with the encode-pass input so no extra readback (T10).
- **Acceptance (done when):** TDD: a content-aware GPU-vs-CPU parity test — render an Image primitive through both the GPU subpass and `blend_image`, assert SSIM/PSNR above threshold (**never bit-exact**, per `multiview-compositor/CLAUDE.md` and the work-stream rule). The naga-validation test (`validate_overlay_shader`) stays green. Re-assert inv #5 (NV12-throughout: the image blits in linear-RGBA in the subpass, output stays NV12; no per-tile RGBA materialization) and inv #8 (color pipeline order unchanged — blend in linear, premultiplied).
- **Risks/notes:** `wgpu` off by default, **no GPU adapter in CI** — the parity/SSIM test must run on a GPU-tagged self-hosted runner (per AGENTS §"Real GPU … run on GPU-tagged self-hosted runners"); keep the WGSL naga-validating and the CPU reference path the CI default. Guardrails: no `unwrap`/`as`/indexing in the pack/upload (the file already hand-rolls as-free float→int helpers, `gpu_subpass.rs:198-264` — follow that style). LGPL-clean (no new deps).
- **Read first:** ADR-0016 §4.1 (overlay subpass position), color-management (premultiplied linear blend), efficiency §2.2 (NV12 policy); the in-file TODO(gpu-image) at `overlay.wgsl:29`.

---

### `[~]` GPU-5 — Multi-GPU PLACEMENT decision engine: closed-loop controller + config + telemetry · effort: XL · deps: none
> **PARTIAL (2026-06-05):** the HAL deliberate-split decision shipped — `crates/multiview-hal/src/split.rs` (`plan_split` + `CutPoint`/`CrossGpuCopy`/`SplitPlan`/`SplitReject`, 8 tests), commit `c995341`. Step 1 (split completeness in `select.rs`) is done. **Remaining:** step 2 (the off-hot-path controller in `multiview-engine` — EWMA/sustained-overload SHED-vs-MIGRATE, make-before-break), step 3 (config policy fields), step 4 (telemetry counters). Track as a follow-on.
- **Goal:** Turn the pure `multiview-hal::select` placement policy (already built) into a live system: an off-hot-path placement controller in `multiview-engine` that senses `DeviceLoad`, detects sustained overload, and proposes SHED-vs-MIGRATE — plus the config policy and telemetry counters ADR-0018 specifies.
- **Touches:** `crates/multiview-hal/src/select.rs` (the pure `select_device` exists — extend with `SplitPlan`/`CutPoint` if absent), `crates/multiview-hal/src/load.rs` (`DeviceLoad`/`LoadPoller` exist, 403). New: a placement controller in `crates/multiview-engine/` (beside the existing `ControlLoop`/`Hysteresis` — confirm names in `engine/src/`), config policy fields in `crates/multiview-config/`, and per-GPU/migration counters in `crates/multiview-telemetry/`.
- **Approach:**
  1. Audit `select.rs` for completeness vs ADR-0018 §2 (pins → hard gates → DRF+Tetris score → tie-break); add the deliberate-split `SplitPlan`/`CutPoint` + `CrossGpuCopy` cost if not yet present.
  2. In `multiview-engine`, add an **off-hot-path** controller that reads the wait-free `Vec<DeviceLoad>` snapshot (published by the engine's poll thread driving `LoadPoller::poll`, `load.rs:433`), runs an EWMA over the existing `Hysteresis` to detect **sustained** overload (transients ignored), and emits a proposal (SHED via the degradation ladder, or MIGRATE via make-before-break at an IDR boundary) — it **only proposes**, never `.await`s the engine (inv #10). Anti-storm damping: cooldown, per-GPU migration budget, min-gain gate.
  3. Add config policy as data (`multiview-config`): per-source/per-output GPU pin (stable `DeviceId`), reserve-headroom, scoring weights (`LoadWeights` already exists, `select.rs:83`), migration cooldown/budget/min-gain — conservative defaults.
  4. Telemetry: reuse the ADR-0017 per-GPU gauges, add placement/migration/split counters; log every adaptation.
- **Acceptance (done when):** TDD (all pure / no hardware — the heart is `select.rs` + a deterministic controller over injected `DeviceLoad`): assert a running pipeline is never fragmented unless no single GPU fits; a split is taken only after degrade-to-fit fails and its copy is cost-accounted; a **transient** spike never migrates while a **sustained** overload does; a migration is IDR-aligned make-before-break dropping zero output frames; anti-storm damps bound migration frequency; a pin always wins; a blind-vendor (all-`None`) probe falls back to VRAM + cost-model without blocking; the controller can never stall the engine (chaos gate). On single-GPU hosts the engine adds zero behaviour. Re-assert inv #1 + inv #9.
- **Risks/notes:** No hardware in CI — the controller and `select` are pure and unit-testable with synthetic `DeviceLoad`s (the `FakeProbe` pattern, `load.rs:716`); real NVML/sysfs probes (`load.rs:495`/`600`) stay feature-gated. Guardrails: no `unwrap`/`as` — `select.rs` already hand-rolls as-free arithmetic; follow it. Largest item; land `select` completeness first, then controller, then config/telemetry, each gate-green.
- **Read first:** ADR-0018 (the whole decision), gpu-placement-engine brief, ADR-0017 (the load model + ranking), ADR-R004 (make-before-break), efficiency §3.

---

### `[~]` GPU-6 — Hardware backend real decode/encode/composite PATHS (cuda/vaapi/qsv/metal) · effort: XL · deps: GPU-3, GPU-1
- **Goal:** Promote the hardware backends from detection/capability-only to real device-resident `decode → composite → encode` islands, bound to the `AVHWFramesContext` scaffold, NV12-throughout, with the single encoder of GPU-1.
- **Touches:** `crates/multiview-ffmpeg/src/hwframe.rs` (the RAII `AVHWDeviceContext`/`AVHWFramesContext` scaffold, the crate's only FFI module, 1-90), `crates/multiview-ffmpeg/src/decode.rs`/`decode_stream.rs`/`encode.rs`/`codec.rs` (bind a real `*_cuvid`/`vaapi`/`qsv`/`videotoolbox` decoder+encoder to the hw frames ctx), `crates/multiview-compositor/src/gpu/` (the native composite fast paths the compositor CLAUDE.md mentions as opt-in), `crates/multiview-hal/src/probe.rs` + `capability.rs` (already detect; now negotiate). Features `cuda`/`vaapi`/`qsv`/`metal` (all currently empty stubs, `compositor/Cargo.toml`).
- **Approach (per backend, NVIDIA first as the reference island):**
  1. Bind a real `get_format`-style decoder callback to the `AVHWFramesContext` (the `hwframe.rs` header flags this as the documented future call site, lines 22-23): `catch_unwind` at the FFI boundary, allocation-light, NV12/P010 sw-format. Decode device-resident (`-hwaccel cuda -hwaccel_output_format cuda`), use NVDEC `cuvid -resize` for decode-at-display-resolution (efficiency §2.1) where one tile size suffices.
  2. Keep the island whole on one device (ADR-0004 zero-copy island; the placement engine GPU-5 decides where): decode→composite→encode all device-resident, **no `hwdownload`/`hwupload` round-trip** — add the telemetry that fails loudly on an inserted scale/convert (ADR-E002 consequence, efficiency §2.5).
  3. Encode through the single hoisted encoder (GPU-1): NVENC P4-P5 low-latency, fixed closed GOP, probe the per-system NVENC session ceiling at runtime (never hard-coded — efficiency §3.2). Mirror per backend: VAAPI/QSV media filters staying in the media stack, VideoToolbox + IOSurface unified-memory zero-copy on Apple.
  4. wgpu interop reality (efficiency §2.5): no stable external-texture import — for non-Vulkan-Video decoders accept the GPU→CPU→GPU round trip until the native path lands; gate the zero-copy claim per backend.
- **Acceptance (done when):** TDD + content-aware: per backend, an integration test on a **GPU-tagged self-hosted runner** decodes a real clip, composites, and encodes once, validated by SSIM/PSNR vs the CPU reference (never bit-exact). Assert inv #5 (NV12 sw-format throughout — `ffprobe`/format-trace shows no inserted RGBA or `hwdownload`), inv #7 (one encode, GPU-1's single-encoder count holds on the hw path), inv #1 (output clock never stalls under decode/encode load), inv #10 (no engine back-pressure). A no-GPU box / default build still compiles and the feature-off arms report graceful absence (the existing `probe`/`load` discipline, `load.rs:495`).
- **Risks/notes:** Heavy hardware + CI dependency — **none of this runs on shared CI**; everything must compile GPU-free (the `hwframe.rs` scaffold already does) and only the real paths run on the self-hosted matrix (AGENTS §CI). Licensing: NVENC/VAAPI/QSV/VideoToolbox are not GPL — LGPL-default stays clean; `gpl-codecs`/`ndi` remain opt-in. Guardrails: `hwframe.rs` is the **only** module allowed `unsafe` (crate is `deny`, not `forbid`) and every block needs `// SAFETY:` (lines 25-30); the `get_format` callback must `catch_unwind`. Sequence after GPU-1 (single encoder) and GPU-3 (describe_output metadata) so the hw encoder slots into the proven egress.
- **Read first:** core-engine §7/§8.1/§12 (hwaccel island), efficiency §2.1/§2.5/§3.2, ADR-E002/E003 (NV12 + encoder selection), ADR-0004 (zero-copy island), color-management; `multiview-ffmpeg/CLAUDE.md` (safety contract).

---

## Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-cli/src/pipeline.rs (consumer_main / StreamEgress / SinkRunner — GPU-1, and synth wiring reference for GPU-2)
- /workspaces/mosaic/crates/multiview-output/src/sink.rs (PacketMuxSink/PacketSource — already built, the GPU-1 target API)
- /workspaces/mosaic/crates/multiview-cli/src/run.rs + /workspaces/mosaic/crates/multiview-cli/src/synth.rs (SoftwareEngine vs generator_loop — GPU-2)
- /workspaces/mosaic/crates/multiview-compositor/src/gpu/gpu_subpass.rs + /workspaces/mosaic/crates/multiview-compositor/src/gpu/shaders/overlay.wgsl (KIND_IMAGE branch — GPU-4; describe_output in gpu/compositor.rs — GPU-3)
- /workspaces/mosaic/crates/multiview-hal/src/select.rs + /workspaces/mosaic/crates/multiview-hal/src/load.rs (placement policy + load model — GPU-5; and /workspaces/mosaic/crates/multiview-ffmpeg/src/hwframe.rs for GPU-6)


## SUR — Captions · NMOS · web codegen

Grounded against: ADR-0016/ADR-0019 + `docs/io/captions.md`; `crates/multiview-control/src/nmos/is05.rs`, `is07.rs`, `nmos/mod.rs`, `nmos/transport.rs`; ADR-RT006 + `docs/api/realtime.md`; `crates/multiview-events/src/envelope.rs`; `web/src/api/layouts.ts`, `web/src/realtime/envelope.ts`, `web/src/app/router.tsx`, `xtask/src/main.rs`, `crates/multiview-control/src/openapi.rs`, `routes/mod.rs`.

Cross-cutting reality check found during exploration (load-bearing for sequencing):
- Layout **write handlers already exist** (`create_layout`/`update_layout`/`delete_layout`/`get_layout`, `routes/mod.rs:96–172`) but carry **plain doc-comments, no `#[utoipa::path]`** → absent from the spec → that is exactly why `layouts.ts` hand-wraps them. Same shape for sources/outputs/overlays (`routes/sources.rs:62–117`).
- `multiview-events` has **no `schemars`/`utoipa` derives** (`envelope.rs:56`) and there is **no AsyncAPI route, no xtask `gen-asyncapi`, no `/asyncapi.json`** anywhere — RT006 is greenfield.
- `is07-mqtt` and `nmos` are **empty feature flags** (`Cargo.toml:106/111`), no broker/timing deps wired.

---

### `[x]` SUR-1 — IS-05 scheduled activation (absolute + relative) · effort: M · deps: none
- **Goal:** Promote a staged IS-05 connection at a scheduled TAI time so a facility controller can pre-load a 2110 receiver swap, completing the activation modes already modelled but deferred.
- **Touches:** `crates/multiview-control/src/nmos/is05.rs` (`ActivationMode` is05.rs:29, `Activation.requested_time` is05.rs:47, `ConnectionState::activate_if_immediate` is05.rs:138); `crates/multiview-control/src/nmos/mod.rs` (`NmosRegistry::stage_connection` mod.rs:156, `RegistryInner` mod.rs:71).
- **Approach:**
  1. Add a total `parse_tai(&str) -> Option<TaiTime>` helper in `is05.rs` over the `<seconds>:<nanoseconds>` form (mirror the focused-parser discipline of `parse_sdp_transport` is05.rs:164 — no `unwrap`, no full RFC clock dep).
  2. Generalise `activate_if_immediate` into `activate_due(now: TaiTime) -> bool` that: applies `ActivateImmediate` unconditionally; for `ActivateScheduledAbsolute` applies when `now >= requested_time`; for `ActivateScheduledRelative` resolves the relative offset against a `staged_at` stamp captured at `stage()` time (add `staged_at: Option<String>` to `ConnectionState`). Keep `activate_if_immediate` as a thin `activate_due` caller so existing call sites/tests stay green.
  3. In `mod.rs`, store the pending scheduled change in `RegistryInner` and add `NmosRegistry::tick_scheduled(now)` that walks `connections` and calls `activate_due`. Drive it from the control plane's existing clock seam (the same boundary that stamps IS-07 `creation_timestamp`, `is07.rs:104` doc) — never from an input PTS (invariant #1).
  4. Surface the staged-vs-active distinction unchanged through `patch_staged` (mod.rs:307) and `get_active` (mod.rs:273); a scheduled stage returns `200` with `staged` populated and `active` unchanged until the tick fires.
- **Acceptance (done when):** new unit tests in `is05.rs` — `scheduled_absolute_activates_at_or_after_requested_time`, `scheduled_relative_activates_after_offset`, `scheduled_does_not_activate_before_due`, `parse_tai_rejects_garbage_without_panicking`; existing `scheduled_activation_is_not_treated_as_immediate` (is05.rs:264) flips to assert it *does* activate once `tick_scheduled` is past due; a `mod.rs` registry test asserts `tick_scheduled` promotes exactly the due receiver. Invariant re-asserted: **#1 output-clock never stalls / #10 no back-pressure** — `tick_scheduled` runs on the control-plane clock, holds only the registry `Mutex` (mod.rs:88) the engine never takes.
- **Risks/notes:** Pure model stays in the always-compiled, CI-green default build (no `nmos` feature, no PTP NIC). No `chrono`/`hifitime` dep needed — TAI seconds:nanos is integer math; guard against overflow with checked arithmetic (no `as`). No `unwrap`/indexing in non-test code.
- **Read first:** `docs/decisions/ADR-… (IS-05 brief, broadcast-multiviewer §6/§8 referenced in is05.rs:1)`; the module doc block at `is05.rs:38–49`.

---

### `[x]` SUR-2 — IS-07 MQTT broker transport · effort: L · deps: none (parallel with SUR-1)
- **Goal:** Carry IS-07 event/tally messages over an MQTT broker so Multiview interoperates with MQTT-native NMOS facilities, completing the transport whose message model + topic seam already exist.
- **Touches:** `crates/multiview-control/src/is07.rs` (`mod mqtt` is07.rs:300, `topic_for_source`/`topic_for_message` is07.rs:317/324, `Is07Message` is07.rs:118); `crates/multiview-control/Cargo.toml` (`"is07-mqtt" = []` Cargo.toml:106); `deny.toml` (opt-in native dep allowance, deny.toml:28).
- **Approach:**
  1. Wire a broker client behind the feature: `"is07-mqtt" = ["dep:rumqttc"]` (async, MIT/Apache — verify against `deny.toml` allowlist; it is *not* a `gpl-codecs`/`ndi`-class dep, so it joins the LGPL-clean opt-in set, but keep it feature-gated out of the default build per the existing comment Cargo.toml:101–105).
  2. In `is07::mqtt`, add a `Publisher` that connects (`rumqttc::AsyncClient`), serialises an `Is07Message` to JSON (reuse the existing tagged serde, no new wire model), and publishes to `topic_for_message(&msg)` (is07.rs:324) at QoS per NMOS guidance. Add a `Subscriber` that subscribes to `x-nmos/events/v1.0/sources/+` and decodes inbound frames back into `Is07Message`, feeding the existing `GpiEvent::from_is07` / `tally_color_from_is07` converters (is07.rs:196/284).
  3. Keep the live socket strictly feature-gated; the **pure topic/codec seam stays always-compiled** (it already is — is07.rs:300 is the only gated block today). Run publish on a detached tokio task off the realtime fan-out so a slow/dead broker cannot back-pressure (invariant #10) — drop-oldest on a bounded channel, identical posture to the WS fan-out (`realtime-api.md §7`).
- **Acceptance (done when):** TDD: pure tests (always run) for round-trip `Is07Message → JSON → topic → decode` already partly exist (`mqtt_topic_follows_the_nmos_convention` is07.rs:493) — extend with a serialise/deserialise-over-the-wire-shape test. Feature-gated integration test behind `#[cfg(feature = "is07-mqtt")]` against an **embedded/in-process MQTT broker** (e.g. `rumqttd` as a dev-dependency) — no external broker in CI; assert a published Boolean state is received and decodes to the right `GpiEvent`. Invariant re-asserted: **#10 no engine back-pressure** — publisher uses a bounded drop-oldest queue, never `.await`ed by the engine; **#1** untouched (control-plane only).
- **Risks/notes:** Network/broker availability in CI — solve with an in-process broker dev-dep, never a live broker. Licensing: confirm `rumqttc`/`rumqttd` license rows are added to `deny.toml` allow-list; default build must stay green with the feature off. No `unwrap`/`as` in the transport task; decode failures degrade to a dropped frame, never a panic.
- **Read first:** `docs/decisions/ADR-RT006.md` (typed-events posture) and the IS-07 module doc `is07.rs:1–28` (transport section is07.rs:19–28).

---

### `[x]` SUR-3 — Caption ingest Phase 2/3: broaden native decode beyond HLS WebVTT · effort: XL · deps: none (Rust, independent of web/NMOS)
- **Goal:** Decode CEA-608/708 (embedded), DVB-teletext, and the HLS native-master rendition in-pipeline per ADR-0019 so captions arrive from the source stream itself, not only from a sidecar/HLS-WebVTT path — degrading to "no cue" and never pacing output.
- **Touches:** `crates/multiview-ffmpeg/` (caption decoder FFI behind `ffmpeg` feature — captions.md §7 table); `crates/multiview-input/` (PMT-walk discovery, A53 sniff, HLS `SUBTITLES`-group resolve, per-tile cue store — captions.md §7; `unsafe = forbid`); `crates/multiview-overlay::subtitle` (`Cue { start, end, lines }` extended with optional `region`, captions.md §1) + `caption_probe` (captions.md §7); `multiview-cli/src/pipeline.rs` (the one `av_read_frame` loop — captions.md §4).
- **Approach:** First **audit what is already real vs missing** (the task explicitly asks this):
  1. Inventory existing surface — sidecar SRT/VTT parser (`multiview-overlay::subtitle::CueTrack`, captions.md §7 "already committed"), the caption-presence probe (already committed), and the HLS-WebVTT path (Phase 1). Confirm the unified `CaptionCue` Text|Bitmap model (captions.md §1) and `CaptionTrackInfo`/selector enum (captions.md §6) exist or scaffold them in `multiview-core`/`multiview-overlay`.
  2. **CEA-608/708 (form 3):** sniff `AV_FRAME_DATA_A53_CC` side data on the already-decoded video frame and feed `cc_dec` (captions.md §3 row 3) — zero extra demux, runs only when `embedded_cc` selector active (captions.md §4 "CEA special case").
  3. **DVB-teletext (form 1) + DVB-sub (form 2):** walk the PMT for `teletext_descriptor`(0x56)/`subtitling_descriptor`(0x59) (captions.md §2 table), instantiate `libzvbi_teletextdec`/`dvbsub` only for the selected page/PID — teletext → Text cue, DVB-sub → Bitmap cue (premultiplied RGBA, captions.md §1).
  4. **HLS native master fetch (form 4 hardening):** resolve the master `#EXT-X-MEDIA:TYPE=SUBTITLES` group and open the rendition's own WebVTT segment list as a *second isolated reader* (captions.md §3 note, §4 diagram).
  5. All forms rebase cue PTS onto the source ns timeline (captions.md §5) and write into the bounded per-tile `CueStore`; the compositor *samples* `active_cue(out_pts)` non-blocking (captions.md §4/§5).
- **Acceptance (done when):** TDD on **synthesised** fixtures built with the FFmpeg CLI (captions.md §9 — never live broadcast): mux known SRT → DVB-sub/teletext TS and assert decoded `CaptionCue`s match; `-a53cc 1` H.264 fixture and assert `cc_dec` recovers captions on the right field/service; generated HLS master+`SUBTITLES` group resolves and cues decode. **ffprobe/content-aware check:** verify the synthesised TS carries the expected subtitle `stream_type`/descriptor before decoding (so the test asserts on real demux, not a tautology). Pure state-machine property tests for cue-store expiry / no-cue gaps / wrong-page-empty / intermittency / rebasing driven by injected `MediaTime` (captions.md §9, ADR-G002). Invariants re-asserted: **#1 output-clock never stalls** (decode on input thread, compositor only reads), **#2 last-good** (no-cue is graceful), **#3 timing** (ns rebasing, no float fps), **#10 isolation** (stalled/wrong-page caption cannot back-pressure; bounded drop-oldest).
- **Risks/notes:** FFI decoders are the only feature-gated part (`ffmpeg` feature) — libzvbi/dvbsub/cc_dec are **already linked** in the FFmpeg 7.1 build, so **no new `cargo deny`-relevant dependency** (captions.md §3). Pure model (cue store, selector, parsers) always compiled+tested with no FFmpeg/GPU. All raw libav FFI stays in `multiview-ffmpeg` with `// SAFETY:` (captions.md §7); `multiview-input` is `unsafe = forbid`. No `unwrap`/`as`/indexing on the decode path. ASS styled rendering (form 6) stays behind the opt-in `libass` feature (ADR-R007) — out of scope here.
- **Read first:** `docs/io/captions.md` (whole doc; §3 decoder table, §4 in-pipeline diagram, §5 timing, §9 tests) and ADR-0019 / ADR-R007 / ADR-R008.

---

### `[x]` SUR-4 — OpenAPI: annotate the layout/resource write ops so they enter the spec · effort: M · deps: none
- **Goal:** Add the `POST`/`PUT`/`DELETE`/`GET-by-id` layout (and sources/outputs/overlays) operations to the generated OpenAPI spec so the web client can call them fully-typed, eliminating the hand-written wrapper's reason to exist.
- **Touches:** `crates/multiview-control/src/routes/mod.rs` (`get_layout` mod.rs:96, `create_layout` mod.rs:108, `update_layout` mod.rs:129, `delete_layout` mod.rs:154 — all currently un-annotated); `crates/multiview-control/src/routes/sources.rs` (`get/create/update/delete_source` sources.rs:62–117); `crates/multiview-control/src/openapi.rs` (`paths(...)` openapi.rs:26, `rest_routes` table openapi.rs:127); `docs/api/openapi.json` (regenerated artifact).
- **Approach:**
  1. Add `#[cfg_attr(feature = "openapi", utoipa::path(...))]` to each write handler — mirror the exact shape of the annotated `list_layouts` (routes/mod.rs:70) and the NMOS `patch_staged` (mod.rs:291): `request_body = LayoutInput`, responses incl. `201`/`200`/`204`, `404`, `412` (`If-Match`), `application/problem+json` body = `Problem`.
  2. Register every new operation in `openapi.rs paths(...)` (openapi.rs:26). The `rest_routes()` table (openapi.rs:129–133) already *lists* `POST/PUT/DELETE /layouts/{id}` — make the actual spec match that asserted truth.
  3. Regenerate: `cargo xtask gen-openapi` (`xtask/src/main.rs:29` → writes `docs/api/openapi.json`).
- **Acceptance (done when):** `crates/multiview-control/tests/openapi.rs` extended to assert the spec now contains `POST/PUT/DELETE /api/v1/layouts/{id}` operations with `LayoutInput` request bodies and `412` responses (it already asserts the route table at openapi.rs:127 — close the gap so the table and the emitted `paths` agree). `cargo xtask gen-openapi` produces a `docs/api/openapi.json` diff containing the new path ops; CI diff-gate (RT006 posture) stays green after commit. Invariant: control-plane only — **#10** unaffected (no new engine interaction).
- **Risks/notes:** Pure annotation work, no behaviour change. Guardrail: keep `Problem` (RFC 9457) as the error body so the generated TS error type is correct. No `unwrap`/`as`.
- **Read first:** `docs/decisions/ADR-W002.md` (utoipa code-first spec) and the existing annotated handler `routes/mod.rs:67–93`.

---

### `[x]` SUR-5 — Web: replace the hand-written layouts wrapper with the generated client + wire deferred routes · effort: M · deps: SUR-4
- **Goal:** Delete `writeLayout`/`deleteLayout` and call the generated `openapi-fetch` client directly, so layout CRUD is compile-checked against the spec, and route the remaining `PlaceholderPage` screens to real surfaces.
- **Touches:** `web/src/api/layouts.ts` (whole file — the `TODO(api-schema)` at layouts.ts:15–17 is the trigger), `web/src/api/schema.ts` (regenerated), `web/src/api/client.ts`; `web/src/app/router.tsx` (route table router.tsx:30); `web/src/pages/PlaceholderPage.tsx` consumers (any route still pointing at it); `web/package.json` (`generate:api` script package.json:9).
- **Approach:**
  1. Regenerate the TS schema from the SUR-4 spec: `npm --prefix web run generate:api` (→ `openapi-typescript ../docs/api/openapi.json -o src/api/schema.ts`, package.json:9). The new `paths` now declare the write ops the wrapper documented as missing (layouts.ts:7–8).
  2. Per the file's own `TODO(api-schema)` (layouts.ts:16): delete `writeLayout`/`deleteLayout`/`LayoutWriteOptions`/`headersFor`/`urlFor`/`readProblem` and call `client.POST/PUT/DELETE('/api/v1/layouts/{id}', …)` through the typed client (`client.ts`), keeping `If-Match`/`ETag` (layouts.ts:88, conventions §6) via the typed `header` params. Keep the exported `Layout`/`LayoutInput`/`Problem` aliases (layouts.ts:25–31) — they already come from generated `components['schemas']`.
  3. Audit which `router.tsx` routes still resolve to `PlaceholderPage`/un-wired screens; wire each deferred `PlaceholderPage` (PlaceholderPage.tsx) to its real page (the CRUD scaffold pattern in `SimplePages.tsx` is the template for resource screens). Update any nav that points at a stub.
- **Acceptance (done when):** Vitest: layouts CRUD tests call through the typed client with no `as`-casts and no bespoke `fetch` (extend the pattern in `web/src/api/queries.test.tsx`); `tsc --noEmit` (part of `npm run build`, package.json:10) passes under strict TS with `writeLayout` removed; `eslint . --max-warnings=0` (package.json:11) clean. Accessibility: each newly-wired route keeps a single `<h1>` + landmark (the `PageHeader` contract PlaceholderPage.tsx:24) and meets **WCAG 2.1 AA** (status by text+glyph, not colour — SimplePages.tsx header comment). Invariant: UI is best-effort, tolerates dropped realtime events (**#10**, web/CLAUDE.md).
- **Risks/notes:** Hard dependency on SUR-4 landing first (the generated schema must contain the write ops, else the client call won't typecheck). Strict TS: no `any`, no non-null assertions; preserve the defensive problem-parsing behaviour the wrapper had. No new hand-written fetch calls (web/CLAUDE.md "do not hand-write fetch calls or types").
- **Read first:** `docs/decisions/ADR-W002.md` / `ADR-W003.md` (generated-client mandate) and `web/CLAUDE.md` (codegen rule).

---

### `[~]` SUR-6 — AsyncAPI generation + generated realtime envelope types (replace the hand-modelled envelope) · effort: XL · deps: none for the schemars derives; the web swap deps SUR-6a
- **Goal:** Implement ADR-RT006: derive AsyncAPI 3.0 from the shared `multiview-events` types, serve `/asyncapi.json`, generate TS models, and replace the hand-modelled `web/src/realtime/envelope.ts` (its `TODO(api-schema)` envelope.ts:3) with codegen — closing the realtime drift gap.
- **Touches:** `crates/multiview-events/` (`envelope.rs:56`, `event.rs`, `subscription.rs`, `topic.rs`, `seq.rs` — add `schemars`/`utoipa` derives; `Cargo.toml` has neither today); `crates/multiview-control/src/openapi.rs` / a new `asyncapi.rs` + route (no `/asyncapi.json` exists yet); `xtask/src/main.rs` (new `gen-asyncapi` task beside `gen-openapi` xtask/src/main.rs:29 — the help text at main.rs:26 already advertises it as "coming soon"); `web/src/realtime/envelope.ts` (+ `connection.ts`, `useEngineEvents.ts`); `web/package.json` (new `generate:events` script).
- **Approach:**
  1. **(SUR-6 Rust)** Add `schemars` (and/or reuse `utoipa::ToSchema`) derives to the `multiview-events` wire types so one set of Rust types feeds both specs (RT006 "single source of truth"). Build the AsyncAPI 3.0 doc in an `asyncapi.rs` (channels = topics, messages = the envelope `oneOf` by `t`), with the documented `serde_json` post-process to inject WS bindings the generator lacks (RT006 Consequences). Serve it at `/asyncapi.json` and add a `gen-asyncapi` xtask that writes `docs/api/asyncapi.json` (mirror `gen_openapi` xtask/src/main.rs:49).
  2. **(SUR-6a web)** Add a `generate:events` script (Modelina or `openapi-typescript`-equivalent for AsyncAPI message schemas) producing typed envelope/payload models; replace the hand-written `Envelope`, `TileState`, `TileSnapshotEntry`, `TileStateDeltaData` and their parsers (envelope.ts:22–213) with generated types behind the thin hand-written runtime (resume/conflation stays hand-owned per RT006 — `connection.ts`/`useEngineEvents.ts`). Keep `parseEnvelope` defensive (tolerate unknown `t`/major, envelope.ts:8) — RT006 keeps the runtime hand-written precisely for this.
- **Acceptance (done when):** TDD Rust: `multiview-control` test asserts `/asyncapi.json` validates against the AsyncAPI 3.0 schema (AsyncAPI CLI in CI) and contains the envelope channels/messages; a serde round-trip test proves the derived schema matches the wire JSON in `docs/api/realtime.md §2`. xtask: `cargo xtask gen-asyncapi` emits `docs/api/asyncapi.json`; **CI regenerates OpenAPI+AsyncAPI+TS and fails on any git diff** (RT006 Decision). Web: Vitest envelope tests pass against generated types; `tsc --noEmit` + `eslint --max-warnings=0` clean. Invariant re-asserted: **#10** — realtime stays best-effort, generated types do not change the no-back-pressure runtime (`realtime-api.md §7`); **#1** untouched.
- **Risks/notes:** RT006 flags `asyncapi-rust` (v0.2) as young and **lacking WS bindings** → the post-process step is mandatory and must itself be tested (schema-assembly fallback from `schema_for!(T)`). Verify `@asyncapi/react-component` renders the exact 3.0 features (keep the WS "Try it" console separate so a docs regression can't break it). Binary meter frames need explicit `contentType` documentation. All-permissive deps (schemars MIT/Apache) keep the default build LGPL-clean. No `unwrap`/`as` in the xtask/generator.
- **Read first:** `docs/decisions/ADR-RT006.md` and `docs/api/realtime.md §2` (envelope) + `docs/research/realtime-api.md`.

---

## Suggested order
SUR-4 → SUR-5 (web layouts swap) form the shortest grounded win. SUR-1 and SUR-2 are independent NMOS items runnable in parallel. SUR-3 (captions XL) and SUR-6 (AsyncAPI XL) are the two largest, each independent at the Rust layer; SUR-6's web envelope swap gates on the SUR-6 Rust spec landing.

## Critical Files for Implementation
- /workspaces/mosaic/crates/multiview-control/src/nmos/is05.rs
- /workspaces/mosaic/crates/multiview-control/src/is07.rs
- /workspaces/mosaic/crates/multiview-control/src/routes/mod.rs
- /workspaces/mosaic/web/src/api/layouts.ts
- /workspaces/mosaic/web/src/realtime/envelope.ts


---

## DEV — Managed devices & display out

**Grounding summary.** Designed 2026-06-10, not yet started. Briefs: [display-out](../research/display-out.md)
(the F1 `display-kms` sink + display nodes + the F3 presentation model) and
[managed-devices](../research/managed-devices.md) (the Devices domain: registry, `zowietek`/`displaynode`/`cast`
drivers, discovery, sync groups). Decisions: ADR-0044/0045, ADR-M008..M011, ADR-RT007, ADR-W017. Four lanes:
**A** Devices-domain core (serial spine A1→A3, then fan out) · **B** display out (B1 is the long pole) ·
**C** timing/sync (after B1; C1 is independent) · **D** Cast (stretch; after A3/A5/D1). Invariant posture: the
display sink consumes the **pre-encode NV12 canvas** via the preview-tap mailbox pattern (the engine never
awaits a sink — inv #1/#10); device drivers are control-plane pollers, so the engine only ever sees ordinary
Sources and Outputs. Coordination points: after **A3+B1** the Devices/`Output` schema surfaces freeze; after
**B5+C2** the F3 acceptance soak gates calling display nodes "frame-accurate". **Hardware gating (updated 2026-06-10):** ZowieBox
(×2, NDI-licensed) and multi-generation Cast validation hardware are available, and the HP t630 test unit is
approved as a dedicated display-node target; only the Raspberry Pi legs remain gated (Pi 4 being provisioned;
no Pi 5 exists).

---

### `[ ]` DEV-A1 — `multiview-config`: `Device` + `SyncGroup` types, validation, export · effort: M · deps: none
- **Goal:** Config-as-code is the durable source for devices: typed `Device` (driver, address, auth `secret_ref`, `desired_mode`, reconnect, display assignment) + `SyncGroup` (mode, `target_skew_ms`, members with per-member `offset_ms`).
- **Touches:** `crates/multiview-config/src/` (new `device.rs`, `sync_group.rs`, registered beside `schema.rs`/`wall.rs`).
- **Acceptance (done when):** serde round-trip (TOML + JSON; tagged unions only — `display.assign` is externally tagged single-key to match the brief §7.3 authored shape, everything else internally tagged, never `untagged`; exact integers, no float; unknown keys rejected at parse) for all three driver variants; validation rejects duplicate ids, unknown drivers, dangling sync-group members, cast sync-group members (Tier D, ADR-M011); export emits desired state only (no runtime status, no discovered-unadopted devices, no ad-hoc Cast sessions; secrets as `ref|redact`). ADR-M008 commits in the same push.

### `[ ]` DEV-A2 — `multiview-events`: `Topic::Devices` + device/timing event types (ADR-RT007) · effort: S · deps: none
- **Goal:** One coarse ids-filtered `Topic::Devices` carrying `device.status` (conflated latest-wins: state/mode/streams/temperature), `device.adopted/.removed/.mode/.error/.sync` (lossless low-rate), `device.discovered` (during scans), plus `TimingStatus` for F3.
- **Touches:** `crates/multiview-events/src/` (topic + event types + envelope registration).
- **Acceptance (done when):** versioned-envelope serde round-trip tests for every new event type; topic ids-filtering tested; no new engine→outside channel (drivers publish from the control plane — inv #10 untouched).

### `[ ]` DEV-A3 — `multiview-control`: devices + sync-groups stores, CRUD, OpenAPI, actions · effort: L · deps: DEV-A1, DEV-A2
- **Goal:** The Devices domain REST surface: registry stores + CRUD + bare-verb actions, OpenAPI-first.
- **Touches:** `crates/multiview-control/src/resource_store.rs` (add `DeviceKind`/`SyncGroupKind` markers), new `crates/multiview-control/src/devices/mod.rs` (registry + state machine), new routes + `openapi.rs` registration.
- **Acceptance (done when):** `/devices` + `/sync-groups` CRUD with ETag/If-Match→412 + Idempotency-Key; `DELETE /devices/{id}` 409s while sources/outputs are bound; bare-verb actions (`probe`, `set-mode` → 202+operation id with declared impact, `reboot` → 202, `identify`/`test-pattern` → 204) per ADR-W017; the DISCOVERED→ADOPTING→ONLINE/DEGRADED/AUTH_FAILED/UNREACHABLE state machine unit-tested; the **conflating broadcaster + session-pump ring rule** (`topic.is_high_rate() || event.is_conflated()`, ADR-RT007) implemented and explicitly tested — resume-after-gap replays lifecycle events losslessly while `device.status` arrives as a fresh snapshot; event `driver` strings constructed only from `multiview_config::DeviceDriver`'s serde wire form, never hand-typed; handlers tested socket-free via `tower::oneshot`; `cargo xtask gen-openapi` regenerated; audit entries on every mutation; IPv6-first examples (bracketed literals).

### `[ ]` DEV-A4 — `zowietek` driver: typed client + poller + three facets (ADR-M009) · effort: XL · deps: DEV-A3
- **Goal:** First vendor driver, clean-room over the vendor-published HTTP API; devices stay a control-plane concern (no engine/`multiview-input` feature).
- **Touches:** new `crates/multiview-control/src/devices/{driver,zowietek}.rs`; per-driver off-by-default feature (`zowietek` / `devices-net` umbrella).
- **Acceptance (done when):** typed JSON-RPC client survives the verified doc hazards (per-device request serialization; backoff on busy codes; `"00000"` vs `"000000"` + `rsp` text drift tolerated; no-HTTP-response on LAN/mDNS/port sets handled as expected reboot); poll-only status ≤1 Hz per status group; the three facets produce **ordinary** managed Sources/Outputs carrying a `device_ref`; `desired_mode` convergence declares device-side (DEV-class) impact before apply (close-before-open semantics); default `cargo check` stays pure-Rust/socket-free with the model compiled + fake-transport tests; bitrate-unit ambiguity recorded as a hardware-verification item, never guessed into the schema. (Hardware leg: typed client validated on a real ZowieBox — **gated on purchase**.)

### `[ ]` DEV-A5 — Discovery infra: mDNS browse + `/discovery/devices` + confirm-adopt · effort: M · deps: DEV-A3
- **Goal:** New shared discovery infrastructure (verified: no working mDNS code exists in-repo) for zowietek/cast/displaynode browsing.
- **Touches:** new mDNS module in `multiview-control` (an `mdns-sd`-class crate behind the net feature), `/discovery/devices` routes.
- **Acceptance (done when):** `POST /discovery/devices/scan` → 202 + results as events; `GET /discovery/devices` returns an **untrusted inventory** requiring explicit confirm-adopt (ADR-0041 doctrine — never auto-ingest); results AAAA-first with IPv4 entries labelled *legacy*; browse task bounded + off the engine path; socket-free tests via an injected browser seam.

### `[ ]` DEV-A6 — SPA: DevicesPage, adopt flow, detail tabs, pickers, help · effort: L · deps: DEV-A3, DEV-A4
- **Goal:** Devices nav entry (between Outputs and Monitoring) + full device management UX.
- **Touches:** `web/src/pages/` (DevicesPage, device detail tabs, Sync Groups page), `web/src/layout/components/SourcePalette.tsx` ("From device" picker), generated OpenAPI client, in-app help.
- **Acceptance (done when):** list shows state badge (never colour-alone), mode, firmware chip, temperature, sync-group chip, last-seen; adopt flow from discovery inventory; detail tabs (Overview/Display/Streams/Sync/Maintenance/Events); Sources/Outputs pickers gain "From device" sections; 202 actions ride `submitOperation`; `/help/devices`, `/help/devices/adopt`, `/help/display-nodes`, `/help/sync` pages exist; vitest component tests + `tsc`/`eslint` clean.

### `[x]` DEV-B1 — `display-kms` sink + `Output::Display` wired end-to-end (ADR-0044) · effort: XL · deps: none (1-day fence spike first: IN_FENCE_FD/OUT_FENCE_PTR via the drm-rs property API; syncobjs fallback)
- **Status:** SHIPPED (branch `ship/dev-b1-display-kms`). Spike verdict: both fence properties ARE expressible via drm-rs 0.15's generic property path (`Value::SignedRange` / `Value::Unknown`), but `OUT_FENCE_PTR` needs raw-pointer plumbing for zero v1 benefit (the v1 producer is the CPU; `PageFlipEvent` timestamps already cover skew telemetry) — **flip-event-only v1**, syncobjs (verified present) reserved for DEV-B3's GPU path. gbm 0.18's BO API suffices for XRGB scanout (`SCANOUT|WRITE|LINEAR` + `map_mut`); its `drm-support` targets drm 0.14, so GEM handles cross via prime-fd export/import (zero `unsafe`). v1 frame path: CPU NV12→XRGB (BT.709, integer 8.8) into GBM-allocated buffers, dumb-buffer fallback (NVIDIA). Hardware-only code is confined to `display/kms.rs`; everything else (mailbox, mode policy + kernel-golden CVT-RB, FlipDriver, sink thread over the `KmsBackend` mock seam) is CI-tested hardware-free. Hardware validation legs (TEST_ONLY probe on real planes, t630/NVIDIA smoke) ride DEV-B2/B3 hardware time.
- **Goal:** The raw-frame DRM/KMS display sink — the long pole of lane B.
- **Touches:** new `crates/multiview-output/src/display/` behind a new off-by-default `display-kms` feature (drm + gbm crates; `cargo deny` green); `crates/multiview-config/src/schema.rs` `Output::Display` threaded through the five exhaustive same-crate matches (`explicit_id`/`gpu_pin`/`audio`/`label`/`validate_outputs`); `crates/multiview-cli/src/pipeline.rs` `build_outputs` (pipeline.rs:3724); the SPA output form (a schema-only edit would be a parseable-but-skipped output — ships fully wired).
- **Acceptance (done when):** a dedicated sink thread owns the DRM fd; flip loop = page-flip-complete → take latest mailbox frame → `atomic_commit(NONBLOCK | PAGE_FLIP_EVENT)`, at most one in-flight commit per CRTC, no-new-frame → do nothing (KMS repeats); a wedged display provably cannot stall the canvas publish (mailbox conflation test — inv #1/#10); `ALLOW_MODESET` never on the frame path; `TEST_ONLY` probes validate plane formats/modifiers at startup; EDID preferred mode chosen by **exact-rational** refresh match (never float fps) + CVT-RB forced-mode fallback for EDID-less heads (per-connector config override); `Output::Display` parses, round-trips, and is built (not skipped) by `build_outputs`; KMS-less CI green via a fake-DRM seam.

### `[ ]` DEV-B2 — HAL/placement scanout affinity · effort: L · deps: DEV-B1
- **Goal:** The display sink is the first GPU-resident raw-frame consumer; the framebuffer must live on the connector-owning GPU.
- **Touches:** `crates/multiview-hal/src/probe.rs` (KMS card-node/connector discovery), `crates/multiview-hal/src/select.rs` (sink-locality constraint), `crates/multiview-engine/src/placement.rs` (migration/split awareness).
- **Acceptance (done when):** probe reports connector-owning card nodes; selection pins composite to the display GPU when a Display sink exists; the placement controller never proposes migrating composite off the scanout GPU (which would force the per-frame GPU→host→GPU copy ADR-0018 forbids) — unit-tested on multi-GPU fixtures; trivially satisfied single-GPU. (Hardware leg: multi-GPU affinity gate exercised on the GPU test server.)

### `[ ]` DEV-B3 — Render path: NV12 direct scanout + wgpu NV12→XRGB fallback · effort: L · deps: DEV-B1
- **Status:** CODE SHIPPED (branch `ship/dev-b3-render-path`), hardware-validation legs pending. The per-hardware buffer-strategy decision is a pure, always-compiled, GPU-free module (`display/strategy.rs`): `select_buffer_strategy` resolves a probed head to **NV12-direct** (Intel Gen9+/vc4, incl. SAND128), the **wgpu XRGB pass**, or the **CPU NV12→XRGB** default; `plane_supports_nv12` gates on format AND the `IN_FORMATS` modifier (the #5727 mis-tile hazard); `parse_in_formats_blob` reads the kernel modifier blob (bounds-checked, `None` not panic on garbage). 14 strategy + 4 canvas-seam unit tests, all GPU-free. The **NV12-direct ADDFB2 path is fully built in `display/kms.rs`** (drm/gbm only — `prime_fd_to_buffer` per plane → `PlanarBuffer` adapter → `add_planar_framebuffer` with the modifier → flip; 0 copies/0 passes; retired-fb teardown; CPU fallback on import failure) and **compiles on the current pin** (drm 0.15 + gbm 0.18). `DisplayCanvas` gained `delivery()` + `dmabuf_image()` (borrowed fds) so a dmabuf canvas opts into the zero-copy path; the shipped CLI `CanvasFrame` is CPU-planes, so the CPU convert runs today. **wgpu-version verdict (RECORDED):** workspace pin is **wgpu 29.0.3**; `SurfaceTargetUnsafe::Drm` + `create_texture_from_hal` + `wgpu_hal::vulkan::texture_from_raw(external_memory…)` all exist, but there is **no safe `texture_from_dmabuf_fd` in v29** (lands later) — dmabuf import on this pin needs `wgpu-hal`+`ash` as new direct deps + raw-Vulkan `unsafe` in this `forbid(unsafe_code)` crate. **Decision: defer the wgpu pass, ship the CPU NV12→XRGB path as the AMD/fallback default** (correct + modest ~0.7 GB/s budget); a whole-workspace wgpu bump to get safe import is the unilateral bump this task says not to make. The `WgpuXrgbPass` variant + `gpu_pass_available` seam compile + are unit-tested; flipping it on is a localized follow-up when the pin advances. Default `cargo check`/`test`/`clippy`/`fmt`/`deny` green with **zero new deps**; `display-kms` build + clippy + tests green. Hardware legs remain: Pi golden NV12-scanout (gated on Pi acquisition); t630 RGB/CPU-path validation; a real dmabuf-canvas to exercise NV12-direct end-to-end on glass.
- **Goal:** Per-hardware buffer strategy, verified per display block.
- **Touches:** `crates/multiview-output/src/display/` (+ wgpu interop, GBM allocation path).
- **Acceptance (done when):** Intel Gen9+/Pi vc4 path does NV12 (Pi: NV12/P030 + SAND modifier) decoder-dmabuf → `ADDFB2` → plane with **0 copies/0 render passes**; AMD DCE11 path (no NV12 scanout exists — confirmed in kernel source) runs exactly one wgpu NV12→XRGB pass into a GBM-allocated scanout buffer (dmabuf import; `SurfaceTargetUnsafe::Drm` fallback), budget documented (~0.7 GB/s @ 1080p60); NVIDIA documented as wgpu-DRM-surface-only tier-2 (no raw-KMS GBM path); the workspace wgpu version pin is decided + recorded. (Hardware legs: Pi golden NV12-scanout test on the deploy kernel — **gated on Pi acquisition**; t630 RGB-pass validation — runs on the dedicated t630 test target.)

### `[ ]` DEV-B4 — ALSA HDMI audio sink (ELD-gated) + buffer servo · effort: L · deps: DEV-B1
- **Goal:** HDMI audio for display heads, reconciling three independent clocks (engine tick, pixel clock, sample clock).
- **Touches:** new ALSA sink under `crates/multiview-output/src/display/` (feature-gated); reuses the adaptive resampler carried in `crates/multiview-audio`.
- **Acceptance (done when):** opens the `hdmi:CARD=…` PCM (sets IEC958 channel status; Pi vc4-hdmi via the alsa-lib card config, not raw `hw:`); ELD-gated via `/proc/asound/cardN/eld#C.P` — audio flows only while the pipe is lit (our always-lit sink guarantees it), EDID-less heads get **no** audio path (documented); bounded audio FIFO + buffer-level servo drives the resampler (the mpv/Kodi display-resample technique); the audio path can never block the flip loop; CI tests over fake ELD/PCM seams.

### `[ ]` DEV-B5 — `multiview node` display-node mode + deployment (ADR-0045) · effort: L · deps: DEV-B1, DEV-B2, DEV-B3, DEV-B4
- **Goal:** Tier-1 display node: the box behaves like a hardware decoder built from commodity hardware, inheriting the product's resilience doctrine for free.
- **Touches:** `crates/multiview-cli/src/` (new `node` subcommand), `deploy/` (systemd unit, container notes), operator docs.
- **Acceptance (done when):** `multiview node` runs one supervised ingest (RTSP/SRT/HLS) → hardware decode → single-source full-canvas → display sink, reusing the **unchanged** multiview-input pacer/jitter/reconnect + framestore tile ladder (last-good, then local slate); systemd unit (`User=multiview` + `video render audio` groups, implicit DRM master on first open, masked getty, watchdog + sd_notify, fbcon restore on crash); container support: rootful `--device /dev/dri --device /dev/snd` takes DRM master and listens on the **kernel** netlink uevent group for hotplug (rootless falls back to `force_probe` polling); ADR-0045 commits in the same push. (Hardware leg: t630 24 h node soak — runs on the dedicated t630 test target.)

### `[ ]` DEV-B6 — Node enrollment/pairing + `displaynode` driver + head binding · effort: M · deps: DEV-A3, DEV-B5
- **Goal:** Nodes become managed devices: enrollment, pairing, and display-head assignment.
- **Touches:** `multiview-control` devices module (`displaynode` driver variant), `/devices/enroll` + `/devices/pair` routes, SPA Display tab, wall-head binding against `crates/multiview-config/src/wall.rs` (`WallConfig`/`HeadConfig`).
- **Acceptance (done when):** enroll = node→controller with a TTL'd token bound to a node keypair; pair = operator completes screen pairing (6-char code + QR); a paired node appears as a device projecting display heads; head assignment binds `{ wall_head }` / `{ program }` / `{ output }` per the config sketch; enrollment tokens one-time-display + hashed at rest.

### `[ ]` DEV-C1 — Outbound presentation epoch (ADR-M010) · effort: L · deps: DEV-A2
- **Goal:** One outbound `WallClockRef` per program so consumers can present against a common wall timeline — the core new outbound timing work.
- **Touches:** `crates/multiview-engine/src/` (epoch derivation from the existing `ptp.rs` servo / `sysref.rs` chrony estimate), reuse of `crates/multiview-core/src/wallclock.rs` `WallClockRef`, `multiview-events` publication, `crates/multiview-output/` (RTCP SR on RTSP — today the crate emits no RTCP; `EXT-X-PROGRAM-DATE-TIME` field on `hls/media.rs` segments; optional RFC 7273 `ts-refclk`/`mediaclk` SDP attributes).
- **Acceptance (done when):** `{stream_id, WallClockRef, link_offset, clock_source/quality}` published per program on the control WS (conflated, drop-oldest — inv #10); RTCP SR NTP↔RTP pairs and HLS PDT are stamped from the **same** epoch; chaos test kills PTP/WS mid-run and the output cadence is unaffected (PTP disciplines an estimate, never paces the tick loop — inv #1); exact-integer arithmetic throughout (never float).

### `[ ]` DEV-C2 — Node presentation discipline + skew telemetry · effort: M · deps: DEV-B5, DEV-C1
- **Goal:** Frame-accurate presentation on nodes: pure pull-side frame choice, never engine feedback.
- **Touches:** the node-mode presentation loop (DEV-B5) + node telemetry.
- **Acceptance (done when):** node decodes into a 2–3-frame queue and at each flip presents the frame whose `wall_at(pts) + link_offset` is closest to the predicted next vblank (KMS vblank timestamps); repeat-if-early/drop-if-late; link offset is a fixed per-deployment receiver delay (uniformity matters, not smallness); WS loss → node keeps the last epoch and free-runs drift-bounded (output never falters); flip-timestamp skew telemetry exported.

### `[ ]` DEV-C3 — Sync groups: apply/measure/test-pattern + drift alarms · effort: M · deps: DEV-A3, DEV-C2
- **Goal:** Sync groups as a first-class resource with honest tier reporting.
- **Touches:** `multiview-control` sync-groups module + actions; events.
- **Acceptance (done when):** achieved tier = **weakest member** (S/A/B/C/D), displayed immediately and never over-claimed; per-member `offset_ms` applies as a node buffer trim at a frame boundary (Class-1; engine cadence untouched); `test-pattern` emits the burnt-in frame counter + flash; a drift alarm raises after `target_skew_ms` + dwell and clears on recovery.

### `[ ]` DEV-C4 — Clock-layer telemetry + deployment guidance + acceptance soak · effort: M · deps: DEV-C2
- **Goal:** Make the clock layer observable and the sync claims testable.
- **Touches:** `crates/multiview-telemetry/` (servo offset/ppb export), deployment docs (ptp4l preferred / chrony fallback configs), the soak harness.
- **Acceptance (done when):** servo offset/ppb exported via telemetry with documented pass thresholds (99th-pct |offset| ≤100 µs PTP / ≤1 ms chrony over 24 h); the acceptance soak — 2 nodes on a non-PTP GbE switch, 24 h, burnt-in counter + photo/OCR validation, plus the invariant-#1 chaos extension (kill PTP/WS mid-soak; cadence unaffected; nodes degrade to free-run) — is scripted and documented. **Soak execution is hardware-gated** (needs ≥2 real nodes; the dedicated t630 test target covers one — the second arrives with the Pi 4).

### `[x]` DEV-D1 — CORS on the HLS HTTP endpoints · effort: S · deps: none
- **Goal:** Cast's Default Media Receiver (and any browser player) fetches playlists/segments cross-origin; CORS is currently absent (`rg`-verified zero hits).
- **Touches:** the HLS HTTP serving surface (the ADR-0032 Cache-Control/CORS header contract) wherever playlists/segments/init are served, + the reference fronting config docs.
- **Acceptance (done when):** playlist/segment/init responses carry the ADR-0032 CORS contract headers; a cross-origin fetch test passes; standalone — no Cast dependency, benefits all browser consumers.
- **As-built:** before this slice nothing Rust served HLS over HTTP at all (the segmenter writes disk files; the compose `nginx:1-alpine` sidecar fronted them header-less), so the slice shipped the delivery surface itself, not just headers: `multiview_output::hls::http` (pure-Rust, default build) — `hls_router(dir)` serves `.m3u8`/`.ts`/`.m4s`/`.mp4`/`.vtt` ONLY (extension allowlist + traversal-safe) with explicit Content-Type, the §6 Cache-Control tiers (playlists `max-age=1, must-revalidate`; segments/init `immutable` 1y), `Accept-Ranges: bytes` + single-range 206/416 (`byte_range`, property-tested), and `with_hls_cors` — the ONE CORS implementation: reflect the request Origin (never `*`) + `Vary: Origin` on every response, `OPTIONS`→204 with the allow set (`GET, HEAD, OPTIONS`; `Content-Type, Range, Accept-Encoding`; max-age 86400), `Content-Length`/`Content-Range` exposed, and NO `Access-Control-*` on Origin-less requests. Wired end-to-end: `bind_and_serve` nests every configured HLS/LL-HLS output at `/hls/{output-id}/` (sanitised id segments, deterministic dedupe) on the control listener via the new `multiview_control::serve_router` composition seam — unauthenticated by construction (outside `/api/v1`, Cast devices cannot send Bearer tokens), inv-#10-safe (file reads only). CORS is on-by-default with no config knob; the future ADR-0032 §7 `cors_allowed_origins` knob narrows the reflected set inside this same layer. `deploy/hls-nginx.conf` is the reference fronting config carrying the identical contract (incl. the `.m4s` MIME type stock nginx lacks), mounted by `deploy/compose.yaml`. Tests: 10 oneshot/property tests (`crates/multiview-output/tests/hls_http_cors.rs`) + the real-socket cross-origin e2e (`crates/multiview-cli/tests/hls_cors_http.rs`).

### `[ ]` DEV-D2 — Cast session actor + `cast` driver (ADR-M011) · effort: L · deps: DEV-A3, DEV-A5, DEV-D1
- **Goal:** Thin control-plane Cast output: point the device at an existing HLS rendition (encode-once preserved; zero engine contact).
- **Touches:** new `cast` driver in `crates/multiview-control/src/devices/` behind an off-by-default `cast` feature; discovery shared with DEV-A5.
- **Acceptance (done when):** the spike outcome is recorded (`rust_cast` 0.21 toolchain check vs a hand-rolled prost/tokio-rustls channel from the BSD-3-Clause Open Screen sources) and one path chosen; session actor: discover `_googlecast._tcp` → TLS:8009 → LAUNCH the Default Media Receiver (`CC1AD845`) → LOAD an H.264 High@≤4.1 + AAC HLS rendition URL (device-reachable/dual-stack — Cast is effectively IPv4-only, documented as conventions-§10 legacy interop); heartbeat per ADR-M011 (PING every 10 s, session dead after 20 s without PONG, reconnect retry every 5 s re-resolving by mDNS UUID), receiver-preempted surfaced; sessions are ephemeral (never exported) with save-as-device promotion; fake-channel tests CI-green; Tier-D (seconds; LL-HLS does not auto-engage) stated in docs/ADR.

### `[ ]` DEV-D3 — SPA cast flow · effort: M · deps: DEV-D2
- **Goal:** Operator-facing casting with honest latency expectations.
- **Touches:** `web/src/` (cast sheet, device list integration), `/help/casting`.
- **Acceptance (done when):** ad-hoc cast sheet (pick device → pick rendition → cast), ephemeral session list with a seconds-class "Tier D" latency badge, save-as-device, manual `address:8009` escape hatch for cross-VLAN mDNS invisibility; component tests + `tsc`/`eslint` clean; help page present.

### `[ ]` DEV-D4 — Cast hardware validation (≥2 device generations) · effort: M · deps: DEV-D2, DEV-D3 · **gated on hardware acquisition**
- **Goal:** Validate the session actor and real glass-to-glass behaviour on physical devices.
- **Touches:** validation notes/docs only (firmware-pinned).
- **Acceptance (done when):** session lifecycle validated across ≥2 Cast device generations on current firmware (versions pinned in the notes); measured glass-to-glass latency documented (expect 6–30 s; LL-HLS does not auto-engage on the Default Media Receiver); failure drills pass (device sleep/IP change → re-resolve by mDNS UUID; sender hijack → "preempted" surfaced). **Blocked until ≥2 Cast devices are acquired.**


---

## TLS / ACME (DNS-01) for the control plane — phased backlog

Subsystem: `multiview-control` (+ `multiview-config`, `multiview-cli`). Design: ADR-0029, brief
[acme-tls](../research/acme-tls.md). Off-by-default Cargo features `tls` (→ `axum-server/tls-rustls`)
and `acme` (→ `instant-acme` + `reqwest/rustls-tls`), wired into `multiview-cli` aggregates. Cloudflare
is the first and (for now) only DNS provider. Dependency-ordered; each phase commits failing tests first
(TDD), keeps default `cargo check` plain-HTTP + LGPL-clean + `cargo deny check` green.

### TLS-0 — static-cert rustls termination (the floor; no ACME)
- New `tls` feature; `serve_tls()` sibling of `serve()` using `axum_server::from_tcp_rustls` over the
  existing bound listener (`tokio::TcpListener` → `.into_std()`), `RustlsConfig::from_pem_file`,
  `axum_server::Handle` for graceful shutdown. Config `[control.tls] mode="static"` +
  `cert_file`/`key_file`. Pure serving glue, no network.
- **Acceptance:** with `mode="static"` and an operator cert, the SPA/REST/WS are served over HTTPS on
  the bound listener; `mode` absent ⇒ unchanged plain HTTP; default build pulls no openssl
  (`cargo deny check` green).

### TLS-1 — ACME core: account + order + DNS-01 flow (provider stubbed)
- New `acme` feature; integrate `instant-acme` 0.8.5: `Account::create`/`from_credentials` with
  `AccountCredentials` persisted `0600` under `/var/lib/multiview/acme/account.<env>.json`; `new_order`
  → walk authorizations → `Dns01` → `key_authorization().dns_value()`; bounded-backoff order polling +
  finalize (`rcgen` CSR) + `certificate()`. DNS publish behind a `DnsProvider` trait whose test impl is
  an in-memory fake. Directory URL config-driven, default **staging**; CI uses Pebble.
- **Acceptance:** against Pebble/staging with the fake provider, a full issue cycle yields a valid cert
  chain + key on disk (`0600`); account creds persist and are reused on a second run (no re-register).

### TLS-2 — `DnsProvider` trait + Cloudflare implementation
- Finalise the object-safe `async_trait` `DnsProvider` (`create_txt_record` → `TxtRecordHandle` newtype,
  idempotent `delete_txt_record`, `wait_for_propagation(deadline)`). Cloudflare impl over
  `reqwest` (rustls-tls, no default features): `POST`/`DELETE /zones/{zone_id}/dns_records`, Bearer
  token, `ttl:60`, optional `GET /zones?name=` when `zone_id` unset. `wait_for_propagation` polls the
  zone's **authoritative** NS for the TXT until present or deadline. Mandatory TXT cleanup on success
  and on failed orders.
- **Acceptance:** an end-to-end DNS-01 issuance against staging using a real single-zone
  **Zone.DNS:Edit** token publishes then deletes the `_acme-challenge` TXT (no residue) and installs a
  trusted leaf with the requested SAN/wildcard.

### TLS-3 — config schema + hot-reload renewal task
- Extend `ControlConfig` (`#[non_exhaustive]`): `Option<TlsConfig>` with adjacently-tagged `mode`
  (`static`|`acme`) and `provider` enums (never `untagged`); `[control.tls.acme]` (`directory`,
  `contact_email`, `domains`, `state_dir`, optional `renewal_lead`) + `[control.tls.acme.cloudflare]`
  (`zone_id`, `api_token_ref = "env:…"|"file:…"|"op://…"` — never the literal token). Validation: exactly
  one of static/acme per `mode`, non-empty `domains`, parseable directory URL, known provider, token ref
  present; reject a literal-looking token. Detached `tokio::spawn` renewal task: renew at
  ⅓-remaining-lifetime + jitter, `RustlsConfig::reload_from_pem` hot-swap, **fail-soft** (warn +
  telemetry + bounded backoff, keep serving existing cert). Metrics
  `multiview_tls_cert_expiry_seconds`, `multiview_tls_renewal_failures_total`.
- **Acceptance:** a config-driven `mode="acme"` run issues at boot and hot-swaps a renewed cert with no
  listener restart and no dropped connections; a forced renewal failure leaves the existing cert serving
  and emits the failure metric; the token is sourced from env/1Password, never logged.

### TLS-4 — hardening, isolation soak, and tests
- Chaos/isolation gate assertion: a stalled/failing ACME order or killed renewal task leaves **program
  output and the existing TLS listener unaffected** (invariant #10 soak). Property/idempotency tests on
  `delete_txt_record` and the renewal scheduler (no duplicate orders on transient failure). Atomic cert
  install (temp+fsync+mode+rename) test; `0600`/uid-10001 permission test. Operator docs: CAA + RFC 8657
  `accounturi`, single-zone token + IP filter, CT-log monitoring, staging→prod promotion runbook,
  account-key/token rotation runbook. Confirm `reqwest` resolves to rustls in `Cargo.lock`.
- **Acceptance:** isolation soak passes (output never stalls under ACME fault injection); mutation/property
  tests green on the renewal + cleanup logic; `cargo deny check` green with `--features tls,acme`; default
  build still plain-HTTP + openssl-free.


---

## Multi-program engine (ADR-0030) — phased backlog

Dependency-ordered. Each phase ships TDD-first (failing test committed separately) and
**re-verifies** the named invariants. MP-0→MP-2 are the serial spine; MP-3/MP-4 and
MP-6/MP-8 fan out once MP-2 lands.

- **MP-0 — Program abstraction + single-program-as-one-Program refactor.**
  Introduce `Program`/`ProgramKind` (engine) + `ProgramSpec` (config types only, no schema
  root change yet); refactor `Pipeline::drive_streaming` body (`pipeline.rs:930–1192`) into
  `MultiviewProgram` owning its clock/drive/egress/stop — a **move**. Add `ProgramId`
  context to `PipelineError`.
  *Acceptance:* existing single-program run + all current invariant tests pass unchanged
  with the run path going through one `MultiviewProgram`.
  *Re-verify:* #1, #7, #10 (no behavioural change).

- **MP-1 — `ProgramSet` supervisor + N independent clocks.**
  `ProgramSet` owns the `ProgramId→ProgramHandle` map + one shared `Arc<dyn TimeSource>`;
  `impl Actor for Program`; `start`/`stop` spawn/stop one task. CLI `main` builds a
  `ProgramSet` instead of one `Pipeline`.
  *Acceptance:* two `Multiview` programs at 25 and 60 fps run concurrently; each
  `ticks_emitted` advances on its own cadence; stopping one leaves the other ticking.
  *Re-verify:* **#1 per program** (independent clocks), **#10** (NEW chaos gate: wedge
  program A's egress, assert program B's `ticks_emitted` keeps advancing on cadence).

- **MP-2 — Decode-once shared `SourceRegistry`.**
  Hoist `stores`+`ingest_plans` (`pipeline.rs:425`) into a process-global ref-counted
  registry keyed by source identity; consumers hold `Arc<TileStore>` clones; ingest
  lifecycle moves to process scope (start on first ref, teardown on last). Decode at the
  supremum requested resolution across consumers.
  *Acceptance:* one source referenced by two programs spins **one** decode actor (assert a
  single ingest task / single decode count); both programs sample last-good lock-free; last
  release tears the source down.
  *Re-verify:* #5/#6 (one decode, supremum res, in-shader downscale), #10 (registry
  lifecycle off the hot path — design-note + no `Drop` in async destructor).

- **MP-3 — Guarded passthrough kind (→ GP-0…GP-12, ADR-0030 §4).**
  *Superseded:* the old "remux + honest `NO_SIGNAL`" plan VIOLATED the operator's
  fail-to-slate rule, so this is now the **guarded passthrough** ladder, broken into
  GP-0…GP-12 (see ADR-0030). `GuardedPacketSource` consumes `Demuxer::read_packet`
  (`demux.rs:196`) on a dedicated egress thread, emitting copied input packets **or** a
  **pre-baked param-matched slate** (black/SMPTE + 1 kHz/silence, encoded once, no held
  session) into the existing encoder-less `PacketMuxSink`s; a degenerate compositor-less
  `EngineRuntime` paces the per-tick liveness decision (watchdog). Prerequisite gaps:
  `Demuxer` `AVIOInterruptCB`+`rw_timeout` (GP-0), strict-IDR `is_idr` classifier (GP-1),
  in-band-PS/Annex-B BSF stage (GP-3). Capability ladder: matched-slate-splice →
  container-discontinuity → full-transcode (`robustness_floor`, default `SlateOnLoss`;
  `PassthroughFallback` removed).
  *Acceptance:* an RTSP H.264 source remuxes to an SRT/MP4 output with **zero** decode/
  encode while healthy (assert no decoder/encoder instantiated); on source loss the program
  emits the pre-baked BLACK/SMPTE+1 kHz slate within `splice_threshold` (≤ ~150 ms) and
  recovers on the next true input IDR — never `is_key`.
  *Re-verify:* **#1 PRESERVED, not excepted** (slate on loss), efficiency (0 decode/encode +
  0 NVENC session at rungs 1–2), #3 (per-stream monotonic clamp+offset, no tick re-stamp),
  #10 (dedicated egress thread + drop-oldest/`SINK_WEDGE_GRACE` detach; two-vector chaos gate).

- **MP-4 — Transcode kind.**
  One source (shared decode from MP-2) → optional scale → `ProgramEncoder` →
  `StreamEgress`; own `OutputClock`, PTS re-stamped from tick. Passthrough→transcode
  fallback path reuses this.
  *Acceptance:* a camera that is both a 3×3 tile **and** a 720p transcode is decoded
  **once** (assert single decode) and produces both outputs; transcode output PTS derives
  from its own tick.
  *Re-verify:* #1 (own clock), #3 (re-stamp from tick), #5/#6 (shared decode, in-shader
  scale), #7 (the transcode rendition is one distinct encode).

- **MP-5 — `[[programs]]` config + backward compat.**
  Add `programs: Vec<Program>` to `MultiviewConfig`; legacy block desugars to one
  `Multiview` program (`id="main"`) via `into_programs()`; `validate()` rejects both-populated;
  `schema_version`→2; cross-program checks (unique ids, unique output labels across programs,
  `input_id` resolves). Round-trip TOML↔JSON.
  *Acceptance:* every existing v1 config parses and produces one `Multiview` program; a new
  3-program config (multiview+passthrough+transcode) validates and round-trips losslessly;
  a config with both legacy + `[[programs]]` is rejected.
  *Re-verify:* config invariants only (no engine change); #7 (per-program outputs).

- **MP-6 — Programs API + realtime.**
  `PROGRAM_KIND` + `routes/programs.rs` CRUD (ETag/If-Match→412, Idempotency-Key, RFC 9457);
  `StartProgram`/`StopProgram`/`ApplyProgram` commands (+ `program` field on `SwapSource`/
  `ApplyLayout`); `…/plan` classification; `Topic::Programs` + `program.state` event;
  `OutputStatus`/`TileState` scoped by `Envelope.id`; snapshot + `CorrKey` extend.
  *Acceptance:* start/stop a single program by id over the API (202+op-id) without affecting
  others; client sees per-program `program.state` (incl. `Migrating` on a Class-2 kind
  change) and a complete snapshot on connect.
  *Re-verify:* **#10** (lifecycle rides the bounded bus, sheds-to-503, never back-pressures
  the engine), #11 (`…/plan` returns Class-1/Class-2; kind change = Class-2).

- **MP-7 — Admission control + global degradation.**
  Union admission crediting shared decode once (de-dup `TileLoad`/source at supremum res)
  through `Planner::admit` + `select_device`; same-source co-location affinity term; add the
  host-IO/bandwidth budget dimension for passthrough; cross-program cheapest-impact shed with
  per-program `priority`.
  *Acceptance:* admitting a program that shares a source does **not** double-count its
  decode; a box at the NVENC ceiling rejects a new transcode with a capability error naming
  the binding resource; under global pressure the low-priority program sheds before the
  primary multiview, and no running program's clock stalls.
  *Re-verify:* **#9** (cross-program shed, bounded drop-never-grow), #1 (shed never stalls a
  running program), ADR-0017/0018 placement gates.

- **MP-8 — UI program list/editor.**
  `programs` nav + routes; `ProgramsPage` (TanStack-Table: kind badge, run-state from
  `program.state`, output count, start/stop→202+job); `usePrograms`; kind-switched editor
  (Multiview = parameterized `LayoutEditorPage`; Passthrough form with remux/transcode
  badge; Transcode form); pre-apply Class-1/Class-2 confirm; multi-program dashboard.
  *Acceptance:* operator creates/starts/stops/edits each of the three program kinds from the
  WebUI; the passthrough editor shows "remux, no re-encode" vs "will transcode"; an A↔B↔C
  kind change shows the Class-2 reset warning before apply.
  *Re-verify:* #11 (live-apply class surfaced before apply); WCAG 2.1 AA.


---

## Workstream FF — Own pinned FFmpeg (replaces jellyfin-ffmpeg / apt / PPA)

Drives [ADR-0031](../decisions/ADR-0031.md). Each phase is a small, reversible PR.
**Gate on FF-0:** keep the binding at `ffmpeg-next = "7.1"` (resolved `ffmpeg-sys-next 7.1.3`,
libavcodec 61) so FF-0 targets **FFmpeg 7.1.4** (same soname 61) and needs **zero Rust-code
change**. The binding *supports* 8.1, but bumping it is FF-0b, not FF-0.

| Phase | Scope | Key files | Acceptance (one line) |
|---|---|---|---|
| **FF-0** | Pin **FFmpeg 7.1.4** (tarball + GPG-verify + SHA-256) + `nv-codec-headers n13.0.19.0`; LGPL-clean `--disable-everything` configure (§4b flags); new `deploy/Dockerfile.ffmpeg` builder stage producing `/opt/multiview-ffmpeg`; wire into `deploy/Dockerfile` (replace apt `libav*`) **and** delete the jellyfin block in `deploy/Dockerfile.nvidia:92-109`. **Shared linkage.** | `deploy/Dockerfile.ffmpeg` (new), `deploy/Dockerfile:71-77,115-124`, `deploy/Dockerfile.nvidia:92-109` | Both images build with **our** libav\* (no jellyfin/apt-libav, no `LD_LIBRARY_PATH` override); `multiview` runs `--locked`; LGPL build exposes `h264_nvenc`/`hevc_nvenc` (no GPL); `ffprobe` present; `cargo deny check` green. |
| **FF-0b** *(gated bump — optional, after FF-0)* | Move `ffmpeg-next = "7.1"`→`"8.1"` in the **three** manifests; `cargo update -p ffmpeg-sys-next`; bump `FFMPEG_VERSION=8.1.1`; update every soname ref (61→62, 59→60, 8→9…) in both Dockerfiles + the `hwdecode.rs:118` comment. | `crates/multiview-ffmpeg/Cargo.toml:29`, `crates/multiview-output/Cargo.toml:53`, `crates/multiview-cli/Cargo.toml:53`, `Cargo.lock`, `deploy/Dockerfile*`, `crates/multiview-ffmpeg/src/hwdecode.rs:118` | `cargo build/clippy/test --workspace --features ffmpeg --locked` green against FFmpeg **8.1.1** (libavcodec 62); both images link `.so.62`. |
| **FF-1** | GPL variant: `FF_LICENSE=gpl` builder ARG → adds `--enable-gpl --enable-libx264 --enable-libx265`; `-gpl` image tag; pairs with the `gpl-codecs` cargo feature. | `deploy/Dockerfile.ffmpeg`, `deploy/Dockerfile*` | `-gpl` image exposes `libx264`/`libx265` and is GPLv2+-labelled; default (non-`gpl`) image has **neither** x264/x265 nor any nonfree lib (`ffmpeg -encoders` / `cargo deny`). |
| **FF-2** | Multi-arch: per-arch native **buildx** for `linux/amd64` + `linux/arm64`; QSV (`--enable-libvpl`) guarded amd64-only; cache builder as a digest-pinned image. | `deploy/Dockerfile.ffmpeg`, CI workflow | `linux/arm64` + `linux/amd64` images build green from one builder def; arm64 omits QSV; builder image cache-hits when ARGs unchanged. |
| **FF-3** | Static-link option: `ffmpeg-sys-next` `static` feature + `FFMPEG_DIR`/`PKG_CONFIG_PATH` at our prefix → single self-contained `multiview` binary, no runtime soname coupling; document the LGPL-2.1 §6 relinkability obligation (satisfied by the shipped source tarball). | `crates/multiview-ffmpeg` build config, `deploy/Dockerfile*` | A statically-linked `multiview` runs with **no** `LD_LIBRARY_PATH` and no libav\* `.so` in the runtime image; `ldd` shows no libav\*; source-tarball artifact published. |
| **FF-4** | Reduce-reliance pilots (feature-flagged, A/B vs libav): (a) pure-Rust **egress muxers** `mpegts`/`flv`/fMP4 displacing `Muxer::create_as` (`multiview-output/src/sink.rs:1109,1163`); (b) pure-Rust **ingest protocols** `retina`/`rml_rtmp`/`srt-tokio` feeding FFmpeg decode. Codecs/hwaccel stay FFmpeg. | `crates/multiview-output/src/sink.rs`, `crates/multiview-input/`, new feature flags | Each pilot passes byte/SSIM parity vs the libav path on golden fixtures behind its flag; libav muxer/protocol unused when the flag is on; FFmpeg surface documented as "codec + hwaccel only." |

---

## Workstream CONSPECT — the account-side subsystem (entitlement · mesh · two-pipe · support)

Drives [ADR-0050](../decisions/ADR-0050.md) (entitlement + enforcement-ladder-as-data + never-off-air),
[ADR-0051](../decisions/ADR-0051.md) (mesh discovery/relay), [ADR-0052](../decisions/ADR-0052.md)
(two-pipe heartbeat/telemetry + consent + retention), [ADR-0053](../decisions/ADR-0053.md)
(support/ticketing + context-pack + data-request approval + audit). Brief:
[conspect-account-architecture](../research/conspect-account-architecture.md).

**Shape of the work.** Two **greenfield** leaf crates (`multiview-licence`, `multiview-mesh`) are built
**first** — they depend only on `multiview-core`(+`events`)(+`licence`), collide with **no** file the
parallel WebUI-owner session touches, and are fully testable against an in-process fake licence
server/peer. The shared surfaces (`multiview-control` routes + OpenAPI, `web/`) come **after** and are
**sequenced behind a single route/OpenAPI-registration owner** (the LANE-API pattern). The **live wire**
(real heartbeat against the licence server, real mesh relay) is **last** and gated on operator
confirmations (brief §14: O1 wire protocol · O2 Ed25519 key-pinning/rotation · O3 claim-code glyph
alphabet · O5 mDNS dependency · O6 salt provenance · O7 tier-gating). **EXACT constants** (brief §2,
used by the external portals — never round): `CLAIM_CODE_TTL`=15m · `CLAIM_REJECT_WINDOW`=24h ·
`TRANSFER_WINDOW`=72h · `ACTIVATION_WINDOW`=31d · `LEASE_FULL`=35d · `LEASE_GRACE`=14d · `LEASE_HARD`=90d
· `FINGERPRINT_MATCH_STRONG`=100 · `FINGERPRINT_MATCH_THRESHOLD`=70 · `CLAIM_CODE_LEN`=6 · Ed25519.

**Dependency order (waves):** `CONSPECT-0 → 1 → {2,3} | 4 | 6` (greenfield fan-out) → `5` → `7 → 8 → 9`
(shared control, single route/OpenAPI owner) → `10` (cli wiring) → `11` (web, behind WebUI coord) →
`12` (live wire + hardware, behind operator confirmations). Critical path:
`0 → 1 → {2,3} → 7 → 9 → 11 → 12`.

### `[ ]` CONSPECT-0 — `multiview-licence` shell: EXACT constants + `LicenceStatus` types · effort: M · deps: none
- **Goal:** Stand up the greenfield leaf crate with the §2 constants as named items, the
  `LicenceStatus`/`Enforcement{level,reasons,since,lease}` published types, and the
  `enforcement.level` enum — the *data* every surface renders. Pure-Rust shell; no network.
- **Touches:** new `crates/multiview-licence/` (lib `multiview_licence`, deps `multiview-core`,
  `multiview-events`, `thiserror`, `tracing`, `chrono`, `ed25519-dalek`); workspace `Cargo.toml`
  members; `conventions §3` crate-table row.
- **Approach:** define the constants module with the §2 values; the `EnforcementLevel`
  `#[non_exhaustive]` enum (`active`/`warning`/`config-locked`/`watermark`/`block-new-instance`/
  `unlicensed-build`); `LicenceStatus` + `Lease` (chrono instants, exact — no float); a `LicenceError`
  enum. Default features empty (pure shell), `heartbeat` reserved for CONSPECT-3.
- **Acceptance:** `cargo check --workspace` + `cargo deny check` stay green with the crate added; unit
  tests assert each §2 constant equals its exact value (e.g. `LEASE_FULL == Duration::days(35)`,
  `FINGERPRINT_MATCH_THRESHOLD == 70`); `LicenceStatus` serde round-trips; `#![warn(missing_docs)]`.
- **Risks/notes:** `ed25519-dalek` + `chrono` must be deny-clean (both `MIT OR Apache-2.0`); keep the
  default build network-free so CI stays pure.
- **Read first:** brief §2, §6.1; [conventions §3](../architecture/conventions.md); [ADR-0050](../decisions/ADR-0050.md) §1–§4.

### `[ ]` CONSPECT-1 — entitlement/lease/claim/transfer/fingerprint state machines (data, not control flow) · effort: L · deps: CONSPECT-0
- **Goal:** Implement the pure state machines that *compute* `enforcement.level` off the hot loop, plus
  the salted-digest fingerprint **score**, all as data — tested with `proptest-state-machine`.
- **Touches:** `crates/multiview-licence/src/{lease,claim,transfer,fingerprint,ladder}.rs`.
- **Approach:** lease arithmetic (§2.2) → level mapping (§6.2); claim
  `Unclaimed→CodeIssued(15m)→{Confirmed→Claimed|Rejected(24h)|Expired}`; transfer
  `Claimed→TransferPending(72h)→{Confirmed→Claimed(new)|Cancelled/Expired→Claimed(old)}`; fingerprint
  = salted component digests → score, threshold 70. **Fail-safe:** on indeterminate input publish
  `warning`, never a harder rung (§6.3 rule 4).
- **Acceptance:** property tests assert the exact constants (15m≠14m; 35d lease; 70 threshold) and the
  level transitions; a "successful heartbeat resets to active" property; an "indeterminate → warning,
  never harder" property; below-70 forces re-claim, ≥70 tolerates drift. No `unwrap`/`as` in non-test
  code.
- **Risks/notes:** clock skew is an *input* to the machine, never an engine concern; model it. Salt
  provenance is operator-confirm O6 — take it as a constructor parameter for now.
- **Read first:** brief §6, §12; [ADR-0050](../decisions/ADR-0050.md) §2, §4, §6, §8.

### `[ ]` CONSPECT-2 — engine seams S1/S2/S3 + the never-off-air chaos test · effort: M · deps: CONSPECT-1
- **Goal:** Wire the three engine seams as **two pre-derived atomics + a startup gate**, and prove
  never-off-air with an executable chaos test (the §6.3 proof made real).
- **Touches:** `crates/multiview-cli/src/run.rs` (S1 at `SoftwareEngine::build()` before
  `EngineRuntime::new()`); `crates/multiview-engine/src/runtime.rs` (S2 frame-boundary control hook
  reads `config_locked`); `crates/multiview-engine/src/drive.rs` (S3 post-composite NV12 watermark
  reads `watermark`); the two `Arc<AtomicBool>` derived from `LicenceStatus`.
- **Approach:** S1 returns a typed `LicenceError` when `level == block-new-instance` (mirror the NDI
  pre-flight gate); S2 skips `apply_layout`/`rebind_cell` when `config_locked`; S3 region-limited NV12
  corner blit (ADR-0023 dirty-rect bake) when `watermark`. No licence logic on the loop — only the two
  atomic loads.
- **Acceptance:** chaos test drives **every** ladder rung (incl. `block-new-instance`) against a
  **running** engine and asserts the output clock still emits exactly N valid frames for N ticks on
  every rung (inv #1); a wedged/crashed entitlement task leaves the last status in place and never
  stalls the loop (inv #10); S2 denies a hot-reconfig while the scene keeps playing; S3 stamps the
  canvas without dropping a tick. Watermark blit is NV12 (inv #5).
- **Risks/notes:** any change that puts lease arithmetic or a network call on the loop is a
  #1-breaking change — forbidden; the seam reads atomics only. Keep the watermark region-limited
  (no full-canvas colour round-trip).
- **Read first:** brief §5, §6.3, §16; [ADR-0050](../decisions/ADR-0050.md) §3, §5; [ADR-I001](../decisions/ADR-I001.md); [ADR-T001](../decisions/ADR-T001.md).

### `[ ]` CONSPECT-3 — heartbeat client (feature-gated) + S4 status surface + lease renew · effort: L · deps: CONSPECT-1
- **Goal:** The `heartbeat`-feature network client that contacts the licence server (monthly minimum),
  renews the lease on success, and publishes `LicenceStatus` wait-free (seam S4). State model stays
  always-compiled; a build with the feature **off** reports `unlicensed-build` honestly.
- **Touches:** `crates/multiview-licence/src/heartbeat.rs` (feature `heartbeat`); the `arc_swap`
  `LatestState<LicenceStatus>` publisher.
- **Approach:** Ed25519-sign the request (salted fingerprint digest + lease request), verify the
  server's signed response, reset the lease to `Active` on success; on failure keep computing the
  ladder from `last_contact_at`. Wire protocol + server key-pinning/rotation are operator-confirm
  O1/O2 — code against a `LicenceServer` trait with an in-process fake.
- **Acceptance:** with `heartbeat` off, the crate compiles and reports `unlicensed-build`; with it on
  (fake server), a successful heartbeat resets the lease to `Active(35d)` and publishes it; a tampered
  server response is rejected. No raw identifier in the request payload (data-minimisation test, §8).
- **Risks/notes:** the honest source-build caveat (§4) — never fake `active` when the client is off.
  The client is off in default/CI, on in shipped presets.
- **Read first:** brief §4, §7, §8; [ADR-0050](../decisions/ADR-0050.md) §3, §7; [ADR-0052](../decisions/ADR-0052.md) §1.

### `[ ]` CONSPECT-4 — `multiview-mesh` discovery: always-on mDNS + signed summaries + untrusted confirm-adopt · effort: M · deps: CONSPECT-1
- **Goal:** The greenfield mesh crate's discovery half — IPv6-first mDNS announce/browse of
  Ed25519-signed salted-digest summaries into a bounded **untrusted** inventory with explicit
  confirm-adopt (the ADR-0041 doctrine).
- **Touches:** new `crates/multiview-mesh/` (lib `multiview_mesh`, deps `multiview-core`,
  `multiview-licence`, `multiview-events`, `tokio`/`mdns` feature, `ed25519-dalek`, `thiserror`,
  `tracing`); workspace members; `conventions §3` row.
- **Approach:** announce/browse a Conspect mDNS service type over `ff02::fb` (IPv4 `224.0.0.251`
  legacy) per ADR-0042; sign the summary (level + lease bounds + salted digest set) — never raw
  identity; populate a bounded untrusted inventory; adopt/forget are explicit operator actions
  (control-plane actor over channels, inv #10). mDNS dependency is operator-confirm O5 — pick a
  deny-clean maintained crate behind the `mdns` feature; the signing/inventory core is always
  compiled + tested.
- **Acceptance:** default build is a pure shell, deny-clean; unit tests assert an announcement carries
  a valid signature + salted digest and **no** raw identifier; a forged-signature announcement is
  rejected; the inventory is bounded (drop-oldest) and never auto-adopts.
- **Risks/notes:** auto-adopt is the classic mDNS footgun — forbidden; confirm-adopt only.
- **Read first:** brief §9.1, §8; [ADR-0051](../decisions/ADR-0051.md) §2, §3; [ADR-0041](../decisions/ADR-0041.md); [ADR-0042](../decisions/ADR-0042.md).

### `[ ]` CONSPECT-5 — mesh relay (end-to-end signed, bounded, dumb carrier) · effort: L · deps: CONSPECT-3, CONSPECT-4
- **Goal:** Relay an adopted offline neighbour's heartbeat to the licence server and carry the signed
  response back — the relayer a **dumb carrier** that cannot read/forge/alter the assertion.
- **Touches:** `crates/multiview-mesh/src/relay.rs`; the bounded drop-oldest relay queue; the cli
  relayer task.
- **Approach:** the originating machine signs the request and the server signs the response
  end-to-end; the relayer forwards bytes it lacks the keys to interpret. Opt-in per machine
  (`PUT /api/v1/mesh/relay`); bounded queue, never blocks on a neighbour; off-hot-loop (inv #10).
- **Acceptance:** a relayer cannot forge or alter an assertion (it lacks the keys — test); a tampered
  relay payload fails the originator/server signature check; a relayed heartbeat resets the offline
  machine's lease exactly as a direct one; the relay queue is bounded drop-oldest and never
  back-pressures. Relayer earns no entitlement from carrying traffic.
- **Risks/notes:** relay framing is shared with the licence-server wire protocol (operator-confirm
  O1/O2) — code against the same `LicenceServer`/fake.
- **Read first:** brief §9.2; [ADR-0051](../decisions/ADR-0051.md) §4, §5.

### `[ ]` CONSPECT-6 — telemetry two-pipe + consent (LWW) + S5 consent-independent retention · effort: M · deps: CONSPECT-0
- **Goal:** Add the **opt-in daily telemetry** pipe (distinct from the heartbeat), the LWW consent
  document, and the **consent-independent** bounded local metrics buffer (seam S5) — all additive in
  `multiview-telemetry`, never co-mingled with the heartbeat.
- **Touches:** `crates/multiview-telemetry/src/{telemetry_pipe,consent,local_metrics}.rs` (additive;
  existing tracing/Prometheus/health untouched).
- **Approach:** consent `{telemetry,updated_at,updated_by}` resolved LWW by `updated_at`; telemetry
  outbound off by default, aggregated/anonymised counters only; the local buffer samples engine
  metrics off the hot loop into a bounded auto-pruned ring, retained **regardless** of consent.
- **Acceptance:** test asserts **no** telemetry field appears in a heartbeat payload and vice-versa;
  turning telemetry **off** stops the outbound pipe but the local buffer keeps populating; no raw
  identifier in either payload (§8); LWW resolves concurrent consent edits by timestamp.
- **Risks/notes:** the separation is a hard directive — the buffer must not be gated on consent; the
  UI/API copy must never present the two pipes as one switch.
- **Read first:** brief §7, §8; [ADR-0052](../decisions/ADR-0052.md) §1–§5.

### `[ ]` CONSPECT-7 — control routes batch A: licence/claim/transfer/fingerprint/enforcement + OpenAPI · effort: L · deps: CONSPECT-1, CONSPECT-3
- **Goal:** The §11 routes 1–15 in **new** `routes/account/*.rs`, registered + documented by the
  **single** route/OpenAPI owner (§15 coordination) so the WebUI session codes against named routes.
- **Touches:** new `crates/multiview-control/src/routes/account/{licence,claim,transfer,fingerprint}.rs`;
  `routes/mod.rs` (registration — single owner); `openapi.rs` (paths + schemas — single owner);
  `state.rs` (account stores). RFC 9457, `ETag`/`If-Match`, `202+operation_id` (heartbeat now),
  `Idempotency-Key` (claim/transfer), RBAC.
- **Approach:** mirror the established CRUD/command patterns; `GET /api/v1/account/licence` renders
  the published `LicenceStatus` incl. `enforcement`; `GET /api/v1/account/enforcement` is the
  chrome-banner read; claim/transfer commands drive the CONSPECT-1 state machines.
- **Acceptance:** per-route tests (CRUD/command + ETag round-trip + If-Match + RBAC + 422 validation)
  mirror `tests/probes.rs`; every route is in `rest_routes()`; `cargo test -p multiview-control openapi`
  green; the fingerprint route returns digests + score, **never** raw identifiers (§8).
- **Risks/notes:** **one owner** registers all account routes + schemas to avoid `openapi.rs`/
  `routes/mod.rs` churn (§15); the WebUI session is told the route/schema names up front.
- **Read first:** brief §11, §15; [ADR-0050](../decisions/ADR-0050.md) §8; control-api scout patterns.

### `[ ]` CONSPECT-8 — control routes batch B: telemetry/consent/metrics/mesh · effort: M · deps: CONSPECT-6, CONSPECT-4
- **Goal:** The §11 routes 16–25 — telemetry/consent/local-metrics + mesh peers/adopt/relay.
- **Touches:** new `routes/account/telemetry.rs` + `routes/mesh.rs`; `routes/mod.rs`/`openapi.rs`
  (single owner); `state.rs` (mesh + telemetry stores).
- **Approach:** consent `GET/PUT` (LWW, `If-Match`); `GET /metrics/local?window=7d`; mesh `peers`
  list + `adopt` (Class-1) + `DELETE`; `relay` `GET/PUT` opt-in.
- **Acceptance:** route tests + ETag/If-Match/RBAC; consent PUT resolves LWW; mesh adopt is Class-1
  and forgettable; metrics-local returns even with telemetry disabled (consent-independent, §7.2);
  all in `rest_routes()`; OpenAPI green.
- **Risks/notes:** same single-owner registration discipline (§15).
- **Read first:** brief §7, §9, §11; [ADR-0052](../decisions/ADR-0052.md); [ADR-0051](../decisions/ADR-0051.md).

### `[ ]` CONSPECT-9 — support: ticketing + context-pack + data-request approval + append-only audit · effort: L · deps: CONSPECT-7, CONSPECT-6
- **Goal:** The §11 routes 26–34 — tickets, the **redacted** context-pack, **data-request local
  approval**, and the **append-only** account audit.
- **Touches:** new `routes/account/{support,audit}.rs`; the support/audit stores (in-memory default,
  sqlite under the `sqlite` feature, ADR-I003 pattern); `state.rs`.
- **Approach:** tickets CRUD + append-only thread; context-pack (`202`) assembled from the local
  metrics buffer + licence/enforcement state + config-as-code with reference-only secrets (ADR-M006)
  — diagnostics only; data-requests as pending actions requiring `approve`/`deny`; audit append-only
  (no update/delete route).
- **Acceptance:** context-pack contains **no** media/raw-identifiers/secret-values (§8 test); **no**
  data egress without an approved data-request (test); audit has no update/delete route and entries
  are immutable + timestamped; every account action (claim/transfer/lease/enforcement/consent/
  context-pack/data-request) is audited.
- **Risks/notes:** reuse the existing audit-log rotation; keep secrets reference-only (ADR-M006).
- **Read first:** brief §10, §11; [ADR-0053](../decisions/ADR-0053.md); [ADR-M006](../decisions/ADR-M006.md); [ADR-W015](../decisions/ADR-W015.md); [ADR-I003](../decisions/ADR-I003.md).

### `[ ]` CONSPECT-10 — cli wiring: spawn the plane tasks + thread the two atomics · effort: M · deps: CONSPECT-2, CONSPECT-3, CONSPECT-4, CONSPECT-6
- **Goal:** Wire the planes into `multiview run`: spawn the heartbeat (S4), local-metrics (S5), and
  mesh announce/relay tasks; derive the two engine atomics (config-lock/watermark) from
  `LicenceStatus`; gate S1 at `SoftwareEngine::build()`. Additive, behind the `account` aggregation.
- **Touches:** `crates/multiview-cli/src/{run,pipeline}.rs` (additive task spawns + atomic threading);
  the `account` cli feature aggregation; shipped presets turn `heartbeat`/`mdns` on.
- **Approach:** spawn the off-hot-loop tasks; publish `LicenceStatus`; derive the atomics; the engine
  reads only the atomics (CONSPECT-2). Default build leaves the network clients off.
- **Acceptance:** `multiview run` with the account plane spawns the tasks and renders licence state at
  `/api/v1/account/licence`; a chaos run (every ladder rung) keeps output on air (re-asserts
  CONSPECT-2 end-to-end); default/CI build stays pure (clients off, `unlicensed-build` honest).
- **Risks/notes:** `pipeline.rs`/`run.rs` are the LANE-CORE long-pole — coordinate with the
  data-plane integrator; keep the additions strictly additive + off-hot-loop.
- **Read first:** brief §5, §13.6; [ADR-0050](../decisions/ADR-0050.md) §5, §9.

### `[ ]` CONSPECT-11 — web/: 7 screens + chrome + generated-client regen · effort: XL · deps: CONSPECT-7, CONSPECT-8, CONSPECT-9
- **Goal:** The 7 account screens (`/welcome`, `/settings/account`, `/licence`, `/data`, `/mesh`,
  `/system/actions`, account-side `/help` nested under DocsLayout) + global chrome (header chip,
  ladder banner, pending-action strip, config-lock interceptor), behind the WebUI-owner coordination.
- **Touches:** `web/src/pages/*` (new screens); `web/src/app/{router,navigation,AppLayout}.tsx`
  (co-review files — extend `NAV_ITEMS` with a grouped Account section, nest `/help`); `web/src/api/
  schema.ts` (regenerated once per merged backend batch by the single owner); i18n catalogs.
- **Approach:** singleton-form pattern (like SettingsPage) for account/licence/data; the **Data**
  screen presents the **two pipes** with distinct copy (§7 — never one switch); the chrome renders
  `enforcement.level` (no-colour-alone, ADR-W011); config-lock interceptor disables resource forms
  (API also returns the lock — UI is courtesy). Reuse the existing `:root` token palette (no new
  tokens). Tile watermark is engine-side — the SPA only explains it.
- **Acceptance:** Playwright e2e drives claim onboarding, a heartbeat-now, a consent toggle (telemetry
  off ⇒ local buffer still shown), a mesh adopt, a data-request approval, and a config-locked
  read-only form; WCAG 2.2 AA; the generated client compiles against the regenerated schema; mobile
  layout preserved.
- **Risks/notes:** the **single coordination point** with the WebUI session (§15) — schema regen is
  single-owner, chrome files are co-review; `/help` collision resolved by nesting under DocsLayout.
- **Read first:** brief §13.7, §15; web-spa scout report; [ADR-W011](../decisions/ADR-W011.md)/[ADR-W012](../decisions/ADR-W012.md).

### `[ ]` CONSPECT-12 — live-wire integration + hardware validation · effort: M · deps: CONSPECT-10, CONSPECT-11
- **Goal:** Replace the fake licence server/peer with the real wire: a real heartbeat against the
  licence server and a real mesh relay between two boxes — gated on the operator confirmations.
- **Touches:** the `LicenceServer` impl (real wire); key-pinning/rotation config; the mDNS dependency
  finalised; salt provisioning.
- **Approach:** implement O1 (wire protocol), O2 (Ed25519 key-pin + rotation), O3 (glyph alphabet
  verbatim), O5 (mDNS dep), O6 (salt provenance) once the operator confirms them; hardware-validate
  on the test fleet (a box that goes offline relays via a LAN neighbour and stays licensed; every
  ladder rung keeps output on air on real hardware).
- **Acceptance:** an official-preset build heartbeats successfully against the real server, renews the
  lease, and survives a server outage by riding `LEASE_FULL`; an offline box relays via a neighbour;
  on-hardware chaos confirms never-off-air on every rung. **Blocked until** the §14 operator
  confirmations land.
- **Risks/notes:** this is the only item with a real external blocker (operator confirmations +
  licence-server availability) — explicitly gated, not parked. Everything upstream is buildable
  against the fake.
- **Read first:** brief §14; [ADR-0050](../decisions/ADR-0050.md)/[ADR-0051](../decisions/ADR-0051.md) operator-confirm sections.
