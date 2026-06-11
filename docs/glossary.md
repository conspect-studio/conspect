# Multiview — Glossary

Domain terms used across the Multiview documentation, alphabetised. Each entry is a single
paragraph; cross-links point to the deep research briefs in [`research/`](research/), the
decision records in [`decisions/`](decisions/), and the architecture docs in
[`architecture/`](architecture/). The canonical names, crate map, feature flags, and
invariants are pinned in [`architecture/conventions.md`](architecture/conventions.md) — where
anything here differs, that document wins.

> **Reading map.** If a term has more depth than one paragraph allows, follow the link. The
> [core-engine brief](research/core-engine.md), [color-management brief](research/color-management.md),
> and [streaming-gotchas brief](research/streaming-gotchas.md) are the primary sources for most
> entries below.

---

## A

**Admission / admission control** — The decision, made before a source or output is started,
about whether the system has the budget (decode megapixels/sec, encode session/chip capacity,
VRAM, bandwidth) to accept it. Admission is the first half of the resource-adaptive control
loop; the second half is degradation. See [`architecture/hardware-and-efficiency.md`](architecture/hardware-and-efficiency.md).

**AVHWDeviceContext** — A libav structure describing a GPU **device** (the CUDA context, the
VAAPI display, the VideoToolbox device, etc.). It is created once per physical device and
shared by every stage pinned to that device so they can stay on one zero-copy island. Lives
behind the safe RAII wrappers in `multiview-ffmpeg`.

**AVHWFramesContext** — The libav structure describing a **pool of GPU frames** (its
`sw_format`, dimensions, and allocation) hanging off an `AVHWDeviceContext`. It is rebuilt by
the decoder's `get_format` callback whenever geometry or `sw_format` change mid-stream; an
app-managed pool can be attached so the compositor shares the decoder's surfaces. See
[`architecture/pipeline.md`](architecture/pipeline.md) and the
[core-engine brief](research/core-engine.md).

## B

**Backend** — A concrete, per-vendor implementation of a HAL stage trait — e.g. an NVENC
encoder, a VAAPI decoder, a Metal compositor. Backends are feature-gated (`cuda`, `vaapi`,
`qsv`, `videotoolbox`, `wgpu`, `metal`) and selected at runtime by negotiation. `software`
is always compiled in as the universal fallback. See
[`architecture/conventions.md`](architecture/conventions.md) §3–4.

## C

**Canvas** — The single output frame the multiview is composed onto: its width, height, fps
(carried as an exact rational such as `60000/1001`), pixel format (NV12 canonical), and
background. The canvas also has a configurable working/output **color space** — default
`sdr-bt709-limited`, with opt-in `hdr-pq-bt2020` / `hdr-hlg-bt2020`. See the
[color brief](research/color-management.md) and [`architecture/color.md`](architecture/color.md).

**Cell** — See **Tile / Cell**.

**Chroma siting** — The spatial position of subsampled chroma samples relative to luma.
4:2:0 H.264/HEVC/MPEG-2 default to `left` (cosited-even horizontal, vertically centered);
JPEG/MPEG-1 use `center`. The compositor must upsample to 4:4:4 with the correct fractional
texel offset or it produces a persistent ~½-chroma-pixel shift. See the
[color brief](research/color-management.md) §4.6.

**Color axes (the 4)** — The four **independent** properties that define a video stream's
color: **primaries** (gamut), **transfer / TRC** (the gamma curve), **matrix / colorspace**
(YUV↔RGB coefficients), and **range** (limited vs full quantization). They are signaled and
defaulted independently and must never be collapsed into one "colorspace" concept. Getting
any one wrong silently corrupts output on some players. Modeled by `ColorInfo` in
`multiview-core`; see the [color brief](research/color-management.md) §1 and
[ADR-C001](decisions/ADR-C001.md).

**Compositor** — The custom GPU stage that scales, places, color-converts, and blends every
tile into the canvas. Multiview deliberately owns this (rather than using FFmpeg filter graphs)
because no FFmpeg/GStreamer stack filter does per-cell fit/cover/crop. Implemented as
`CudaCompositor` / `MetalCompositor` / `VulkanCompositor` (with wgpu as the portable
baseline). See [`architecture/pipeline.md`](architecture/pipeline.md) and the
[core-engine brief](research/core-engine.md) §8.2.

## D

**Deadline-driven compositor** — A compositor that produces a frame at each fixed output
deadline using whatever tiles are currently available, **never** waiting for all inputs (one
stalled source must never freeze the multiview). Stalled tiles render their last-good frame, then
a "no signal" card after a stale timeout. Mirrors `GstAggregator`'s deadline model; note the
GStreamer default is wait-for-all, so the deadline path is deliberately engineered as the
primary mode. See [`architecture/timing-and-sync.md`](architecture/timing-and-sync.md) and the
[streaming-gotchas brief](research/streaming-gotchas.md) §1.

**Decode-at-display-resolution** — The efficiency rule that each source is decoded near its
displayed tile size where the backend supports it (NVDEC `-resize`, VideoToolbox/VAAPI/QSV per
the capability matrix), or by preferring a smaller source rendition/substream — so decode
budget is spent in megapixels/sec, not full-res-everywhere. See
[`architecture/hardware-and-efficiency.md`](architecture/hardware-and-efficiency.md).

**Degradation (resource-adaptive)** — A closed control loop (sense → estimate → plan → apply,
with hysteresis) that sheds load **tile-by-tile** in a defined cheapest-impact-first order
*before* the program output is ever touched. Bounded queues drop rather than grow. The
counterpart to admission. See [`architecture/hardware-and-efficiency.md`](architecture/hardware-and-efficiency.md)
and [`architecture/conventions.md`](architecture/conventions.md) §5.9.

**DTS (Decode TimeStamp)** — The timestamp telling the decoder/muxer *when to decode* a
packet, which differs from PTS (display order) whenever B-frames reorder frames. Multiview
schedules by `best_effort_timestamp` (display order) on input and lets libavcodec assign DTS
on the encoded output; any stream-copy path must clamp `dts = max(dts, last+1)` because
`av_interleaved_write_frame` aborts on the first non-monotonic DTS. See
[`architecture/timing-and-sync.md`](architecture/timing-and-sync.md) and the
[streaming-gotchas brief](research/streaming-gotchas.md) §2.

## E

**EBU R128** — The loudness-normalisation standard (integrated LUFS, loudness range, true
peak) used by `multiview-audio` for metering the program bus and discrete tracks. High-rate
meters are sampled/conflated (~10–30 Hz) before going to clients so the realtime layer never
back-pressures the engine. See [`architecture/conventions.md`](architecture/conventions.md) §3.

**EBU R37** — The lip-sync tolerance window (audio +40 ms ahead / −60 ms behind video).
Multiview biases audio slightly behind video because audio-ahead is more perceptible, and uses
this window as a soak-test acceptance criterion. See the
[streaming-gotchas brief](research/streaming-gotchas.md) §5, §7.

**Encode-once-mux-many** — The core efficiency invariant: composite once, encode the canvas
once per *rendition* (codec/resolution/bitrate), then fan the *same* encoded packets out to
all transports (RTSP, HLS, RTMP, SRT). Separate encodes are created only when codec, resolution,
or bitrate genuinely differ. See [`architecture/hardware-and-efficiency.md`](architecture/hardware-and-efficiency.md)
and [`architecture/conventions.md`](architecture/conventions.md) §5.7.

**EOTF (Electro-Optical Transfer Function)** — The curve that maps code values to linear
display light (e.g. BT.1886 for broadcast video, sRGB for graphics, PQ/HLG for HDR). The
compositor **linearizes** each tile via its EOTF before any scaling, blending, or primaries
math. Its inverse, the **OETF**, is applied on encode. See the
[color brief](research/color-management.md) §4.4.

## F

**Fit mode** — The CSS `object-fit`-style policy `{fill, contain, cover, none, scale_down}`
(plus an `align`/anchor) describing how a source maps into its cell. Cover/none crop the
source (src-rect); contain/scale_down pad the destination (letterbox). Fit modes lower to
(src-rect, dst-rect) pairs in the compositor. See the [core-engine brief](research/core-engine.md) §13.

**Frame** — The canonical media unit in `multiview-core`, carrying a backend-tagged surface
handle (CUDA pointer + pitch / wgpu texture / IOSurface / host buffer), its `PixelFormat`,
its resolved `ColorInfo` (the 4 axes), and its `MediaTime`. Channels carry frame *handles*,
never pixels. See [`architecture/conventions.md`](architecture/conventions.md) §3.

**Frame store** — The per-tile **single-slot, lock-free** store (a triple-buffer in
`multiview-framestore`) into which an input writes its newest decoded frame and from which the
compositor reads the latest at each output tick. Overwrite semantics give bounded memory
(newest wins); the store never blocks the compositor. See
[`architecture/pipeline.md`](architecture/pipeline.md) and the
[streaming-gotchas brief](research/streaming-gotchas.md) §0.

**FrameSync / TBC (Time-Base Corrector)** — The per-source mechanism that converts push
delivery to pull ("give me the frame valid at running_time T") and absorbs jitter and
inevitable crystal drift via continuous drop/repeat (video) and adaptive resampling (audio).
Modeled on the NDI framesync / broadcast TBC. See [`architecture/timing-and-sync.md`](architecture/timing-and-sync.md).

## H

**HAL (Hardware Abstraction Layer)** — The backend-agnostic trait layer (`multiview-hal` plus the
stage traits in `multiview-core`: `Source`, `Sink`, `Decoder`, `Encoder`, `Compositor`,
`Backend`) that lets each pipeline stage be negotiated independently across vendors. It also
owns capability detection, the backend registry, and the cost-model planner. See
[`architecture/overview.md`](architecture/overview.md) and the
[core-engine brief](research/core-engine.md) §6.

**hwaccel** — The generic FFmpeg path in which a software-named decoder delegates to hardware
via an `AVHWDeviceContext` plus a `get_format` callback, yielding GPU-resident frames and
supporting automatic software fallback. Multiview prefers this generic path over the `*_cuvid` /
`*_qsv` wrapper decoders precisely because only it offers uniform negotiation and fallback.
See the [core-engine brief](research/core-engine.md) §6.5.

## L

**Last-good frame** — The most recent valid frame retained per tile (in VRAM, alongside the
frame store) so the compositor can hold it when a source stalls, bursts, or delivers corrupt
frames — and continue emitting valid output forever. After a configurable stale timeout the
tile escalates to a placeholder / "no signal" card. See
[`architecture/resilience.md`](architecture/resilience.md) and
[`architecture/conventions.md`](architecture/conventions.md) §5.2.

**Limited range / full range** — The two YUV quantization swings. **Limited** ("TV"/MPEG,
`AVCOL_RANGE_MPEG`=1) uses Y′ 16–235 and Cb/Cr 16–240 (8-bit); **full** ("PC"/JPEG,
`AVCOL_RANGE_JPEG`=2) uses 0–255. Range is one of the highest-impact *and* most common color
bugs: limited-as-full gives elevated/grey blacks and washout; full-as-limited crushes blacks
and clips whites. Range is expanded **once** before the YUV→RGB matrix and compressed **once**
on encode. Note the unspecified sentinel for range is **0** (it is **2** for the other three
axes). See the [color brief](research/color-management.md) §1–2 and [ADR-C002](decisions/ADR-C002.md).

**LL-HLS (Low-Latency HLS)** — Apple's low-latency HLS: partial segments (`EXT-X-PART`),
preload hints, `EXT-X-SERVER-CONTROL` with `CAN-BLOCK-RELOAD`/`PART-HOLD-BACK`, and blocking
playlist reload (`_HLS_msn`/`_HLS_part`) over HTTP/2. FFmpeg's `hls` muxer **cannot** emit it
(its `-lhls` is the unrelated `EXT-X-PREFETCH` variant), so Multiview builds a custom CMAF
segmenter + blocking-reload origin in `multiview-output`, reusing the `hls-playlist` crate for the
tag layer. Target latency ~2–5 s. See [`io/outputs.md`](io/outputs.md), the
[streaming-gotchas brief](research/streaming-gotchas.md) §4, and the
[core-engine brief](research/core-engine.md) §9.2.

**Linear light** — The radiometrically-linear RGB space (light intensity, not gamma-encoded
code values) in which all scaling and premultiplied-alpha blending happen. Compositing in
gamma/YUV space causes dark fringing on edges and wrong mids. The canvas working buffer is
`Rgba16Float`. See the [color brief](research/color-management.md) §2 and
[`architecture/color.md`](architecture/color.md).

## M

**MediaTime** — Multiview's internal monotonic timeline value, carried as i64 nanoseconds (NTSC
`1001` rates kept as exact rationals/ns, never float fps). Per-input PTS is normalized and
rebased onto this single timeline, and the output re-stamps all PTS/DTS from the tick counter.
Defined in `multiview-core`. See [`architecture/timing-and-sync.md`](architecture/timing-and-sync.md).

## N

**NDI (Network Device Interface)** — Vizrt's IP video transport, a first-class but isolated
input and output. The SDK is **proprietary** and royalty-free (attribution required,
redistribution restricted), never vendored; the `ndi` feature is off by default and uses a
runtime `NDIlib_v6_load()` dynamic-load path. NDI frames are host-memory (UYVY/P216/BGRA), so
there is always one host→GPU upload on ingest and one GPU→host copy on output, and NDI carries
**no** CICP color tags — YUV is by-convention limited, RGBA full. See
[`architecture/conventions.md`](architecture/conventions.md) §7, the
[core-engine brief](research/core-engine.md) §10, and the
[color brief](research/color-management.md) §6.3.

**Negotiation** — The per-stage, runtime selection of backends as a constrained assignment
problem: each candidate carries a static rank, a measured cost, and hard constraints (codec/
resolution support, NVENC session budget). The planner minimises total cost, adding a large
penalty for crossing a vendor/device boundary and a bonus for staying on one device end to end.
See the [core-engine brief](research/core-engine.md) §6.3, [ADR-0003](decisions/ADR-0003.md),
and [ADR-E008](decisions/ADR-E008.md).

**NV12 / P010** — YUV pixel layouts. **NV12** is 8-bit 4:2:0 semi-planar (1.5 B/px) and is
Multiview's canonical pixel format — frames stay NV12 throughout and YUV→RGB happens in-shader at
tile size; RGBA is never materialised per tile. **P010** is the 10-bit equivalent (10 bits
stored in the high bits of 16-bit words; descale before normalizing). Related host layouts:
P216 (16-bit 4:2:2), UYVY (packed 4:2:2). See
[`architecture/conventions.md`](architecture/conventions.md) §5.5 and the
[color brief](research/color-management.md) §4.2.

## O

**OETF (Opto-Electronic Transfer Function)** — The encode-side curve mapping linear light to
code values (e.g. BT.709 OETF, the inverse of the canvas TRC). Applied once on encode after
linear-light compositing. Note BT.709 OETF ≠ BT.709 EOTF ≠ sRGB — do not reuse one as the
other. See the [color brief](research/color-management.md) §4.4.

**Output-clock invariant** — The load-bearing rule that at every tick of a single fixed-cadence
internal monotonic clock, the output stage emits exactly one valid, correctly-timestamped frame
(plus matching audio), **forever**, independent of any input. Inputs are *sampled*, never
*pacing*; output PTS = `f(tick)`. This is the foundation of bulletproof continuous output. See
[`architecture/timing-and-sync.md`](architecture/timing-and-sync.md), [`architecture/resilience.md`](architecture/resilience.md),
and [`architecture/conventions.md`](architecture/conventions.md) §5.1.

## P

**Pacer (input pacer)** — A custom PTS-to-wall-clock pacer placed between demux and the frame
store for live/VOD-as-live inputs (especially HLS, which delivers a multi-segment backlog on
connect). It releases frames at `now ≥ anchor_wall + (pts − pts0)` from a bounded ring,
re-anchors on discontinuity, and caps catch-up at ~1.25×. FFmpeg's `-re` is for files, not live
ingest. See the [streaming-gotchas brief](research/streaming-gotchas.md) §3 and
[`io/inputs.md`](io/inputs.md).

**PTS (Presentation TimeStamp)** — The timestamp saying *when a frame is displayed*. Per-input
PTS is normalized (33-bit/RTP wrap unwrap, genpts fallback, monotonic guard, discontinuity
re-anchor) and rebased onto the internal ns timeline; the output then re-stamps every PTS from
the tick counter and never propagates raw input PTS to the muxer. See
[`architecture/timing-and-sync.md`](architecture/timing-and-sync.md) and the
[streaming-gotchas brief](research/streaming-gotchas.md) §0, §2.

## R

**Range** — See **Limited range / full range** and **Color axes**.

## S

**SFE (Split-Frame Encoding)** — NVIDIA's technique for encoding a single high-resolution
stream across multiple NVENC engines (HEVC/AV1, Ada+). Relevant because encode density is bound
by physical NVENC *chips*, not the session-count headline — most GeForce SKUs have one NVENC.
See the [core-engine brief](research/core-engine.md) §8.3 and §3 glossary.

**sw_format** — The real memory layout (NV12 / P010 / P016 / …) behind an opaque hardware
surface format (`AV_PIX_FMT_CUDA` / `VAAPI` / `QSV` / `VIDEOTOOLBOX`). The decode path branches
on `sw_format` for plane layout and bit depth, and it can change mid-stream (handled in
`get_format`). See the [core-engine brief](research/core-engine.md) §3, §8.1.

## T

**Tile / Cell** — One source rendered into a sub-rectangle of the multiview canvas. Each tile owns
its own decode→FrameSync chain, frame store, last-good frame, and resolved color tuple, and
rides a state machine independently of every other tile. See **Tile state machine** and the
[core-engine brief](research/core-engine.md) §3.

**Tile state machine** — The per-tile lifecycle **LIVE → STALE → RECONNECTING → NO_SIGNAL**
that governs what the compositor draws: the latest frame when LIVE, the last-good frame while
STALE/RECONNECTING, and a placeholder card at NO_SIGNAL. Lives in `multiview-framestore`. See
[`architecture/resilience.md`](architecture/resilience.md) and
[`architecture/conventions.md`](architecture/conventions.md) §5.2.

**Timebase** — The rational unit (e.g. `1/90000`, `1/1000000000`) in which a stream's
timestamps are counted. Multiview rescales every input timebase to its internal ns timeline via
`av_rescale_q` and carries NTSC `1001`-family cadences as exact rationals — never float fps,
which drifts ~3.6 s/hour. See the [streaming-gotchas brief](research/streaming-gotchas.md) §2.

**Tone-mapping** — Mapping HDR tile values down into the SDR canvas (or SDR up into an HDR
canvas), **per-tile in linear light**, with a roll-off anchored at BT.2408 reference white
(203 cd/m²) — not a linear scale to peak, which washes out the SDR tiles. Default algorithm is
the temporally-stable BT.2390 EETF. See the [color brief](research/color-management.md) §5 and
[ADR-C005](decisions/ADR-C005.md).

**Transfer / TRC** — See **Color axes** and **EOTF / OETF**. The opto-electronic curve (gamma)
mapping linear light to code values; `AVColorTransferCharacteristic`, unspecified = 2.

## W

**WHEP (WebRTC-HTTP Egress Protocol)** — The sub-second-latency, signed-token-gated transport
used by the isolated **preview** subsystem (`multiview-preview`, `webrtc` feature) for input/
program/output taps. Preview is physically incapable of back-pressuring the engine and
auto-stops with no subscribers; cheaper MJPEG/JPEG grids cover the high-density case. WebRTC is
*not* a program-output transport in v1. See [`architecture/conventions.md`](architecture/conventions.md) §6
and the [preview brief](research/preview-subsystem.md).

## Z

**Zero-copy island** — A single GPU vendor/device within which frames move
decode → composite → encode with **no host copy** (NVDEC→CUDA→NVENC sharing one CUDA context;
VideoToolbox→IOSurface→Metal→VideoToolbox; VAAPI/QSV→Vulkan via dma-buf). Cross-vendor on-GPU
zero-copy **does not exist on desktop**, so the architecture treats each vendor as an isolated
island and budgets an explicit `av_hwframe_transfer_data` copy at every vendor/device boundary
and at every NDI/host seam. See the [core-engine brief](research/core-engine.md) §7 and
[ADR-0004](decisions/ADR-0004.md).

---

*See also: [`architecture/conventions.md`](architecture/conventions.md) (source of truth),
the [research briefs index](research/README.md), and the [ADR index](decisions/README.md).*
