# HDMI/display output — DRM/KMS scanout sinks + display nodes

> **Status:** design brief (verification-hardened, 2026-06-10) for an **unbuilt** feature — every
> code path named as *existing* below was verified with `rg`/read in this repo; everything else is
> labelled proposed. **Decisions:** [ADR-0044](../decisions/ADR-0044.md) (the `display-kms` sink:
> DRM/KMS scanout, mailbox flip policy, scanout affinity), [ADR-0045](../decisions/ADR-0045.md)
> (display-node mode: `multiview node`, enrollment, node presentation discipline, the
> display-locked clock option), [ADR-M010](../decisions/ADR-M010.md) (multi-output timing & sync
> tiers). **Companion brief:** [managed-devices](managed-devices.md) — device registry, enrollment/
> pairing, vendor-decoder drivers; this brief owns the *media path* (decode → scanout → audio),
> that one owns the *management path*.
>
> **Builds ON (reference, not duplicated):** [ADR-T001](../decisions/ADR-T001.md) output clock
> (`multiview-engine/src/clock.rs` `OutputClock`, `out_pts = MediaTime::from_tick`); the framestore
> ladder (`multiview-framestore/src/tile.rs` `TileStore::read_at`, `RING_CAPACITY = 256`,
> [ADR-T002](../decisions/ADR-T002.md)); the preview-tap isolation contract
> (`multiview-preview/src/lib.rs` `TapRegistry`/`TapLease`, drop-oldest, engine never awaits a
> viewer); the ingest stack (`multiview-input/src/{pacer,jitter,normalize,reconnect}.rs`); the PTP
> servo + wall-clock reference (`multiview-engine/src/ptp.rs` `PtpServo`/`ReferenceTracker`,
> `multiview-core/src/wallclock.rs` `WallClockRef`, [ADR-0038](../decisions/ADR-0038.md),
> [wall-clock-sync](wall-clock-sync.md)); affinity-first placement
> ([ADR-0018](../decisions/ADR-0018.md), `multiview-engine/src/placement.rs`); the wall/head model
> (`multiview-config/src/wall.rs` `WallConfig`/`HeadConfig`, `multiview-engine/src/heads.rs`,
> [ADR-MV001](../decisions/ADR-MV001.md)).
>
> **Invariants touched:** #1 (output clock — the sink is sampled, never pacing; §7 argues the one
> deliberate node-only exception), #2 (last-good ladder — display nodes inherit it), #5
> (NV12-throughout — the scanout buffer is the one place RGB is *mandatory* on some hardware, in
> exactly the in-shader-once way ADR-E002 prescribes), #7 (encode-once — the display sink is
> pre-encode and changes nothing about packet fan-out), #9 (degradation budgets the decode
> ceiling), #10 (isolation — the sink physically cannot stall the engine), #11 (modeset = Class-2).

Multiview gains a **first-class picture on real glass**: HDMI/DisplayPort outputs driven directly
from DRM/KMS on headless Linux, with no X11/Wayland anywhere. Two tiers, one codebase:

- **Tier 1 — display node** (`multiview node`): a small box (thin client, Raspberry Pi) behaves
  like a hardware decoder built from commodity parts. One supervised ingest (RTSP/SRT/HLS from a
  central Multiview) → hardware decode → KMS atomic scanout + ALSA HDMI audio. This is the
  "documented external IP gateway" the roadmap reserves for physical outputs, made a product
  component ([ADR-0045](../decisions/ADR-0045.md)).
- **Tier 2 — local multiview with attached displays**: the full engine runs locally and one or
  more `Output::Display` sinks scan the composited canvas out to connectors. Feasible on a
  thin-client class box at a modest tile count (§11); first-class on bigger hardware.

The node-side **presentation model** (which frame goes on which vblank, shared-wall-clock frame
choice, audio servo) lives here too, because it is inseparable from the flip loop; the *outbound
epoch* it consumes is specified in [ADR-M010](../decisions/ADR-M010.md) and amends
[wall-clock-sync](wall-clock-sync.md).

---

## 0. Where the sink sits (and why it is a raw-frame sink)

The production encode-once fan-out today is the CLI pipeline's `build_outputs`
(`crates/multiview-cli/src/pipeline.rs`) feeding `multiview-output` `PacketMuxSink`s (HLS,
RTMP/SRT, file); `fanout.rs`'s `PacketRouter` is a parallel model not on that wired path. A display
sink must **not** join the packet fan-out: it consumes the **pre-encode NV12 canvas**, exactly like
a preview tap. The proven isolation shape is `multiview-preview`'s tap contract — the engine
publishes frames it already produced into a wait-free latest-frame slot; consumers subscribe
drop-oldest; the engine never awaits anyone. The display sink reuses that pattern with one
difference: it is *always on* (a display is not a transient viewer), so it owns a dedicated
mailbox rather than a refcounted tap lease.

Consequences, all by construction:

- **Invariant #7 untouched** — no new encode, no new packet path. A box that both displays locally
  and serves HLS still encodes once.
- **Invariant #10 holds** — publishing into the mailbox is wait-free; a wedged display thread (or
  a kernel stuck in a slow commit) can never back-pressure the tick loop. The CI chaos gate for
  taps extends to the display sink (§13).
- **Invariant #5 holds** — frames stay NV12 up to the last possible moment; where hardware cannot
  scan out NV12 (§2), the one conversion happens in-shader at canvas size, which is precisely the
  ADR-E002 rule ("YUV→RGB in-shader, exactly once").

## 1. Scanout architecture — drm-rs + gbm behind `display-kms`

**New module `crates/multiview-output/src/display/` behind a new off-by-default feature
`display-kms`** (proposed; nothing exists today — `rg` finds no `drm`/`gbm`/KMS code in the
workspace). Dependencies:

- **`drm` crate 0.15 (Smithay, MIT, pure-ioctl)** — no C linkage, so the *crate* could compile in
  the default build; we still gate it because the feature is meaningless without hardware and the
  default build stays dependency-minimal. Verified API coverage (docs.rs): `atomic_commit` with
  `AtomicCommitFlags` (`NONBLOCK`, `ALLOW_MODESET`, `TEST_ONLY`, `PAGE_FLIP_EVENT`),
  `add_planar_framebuffer` (ADDFB2 **with format modifiers**), typed property get/set, the full
  syncobj suite, `create_lease`, `get_connector(force_probe)`, and an `Events` iterator delivering
  `VblankEvent`/`PageFlipEvent` with sequence + timestamp.
- **`gbm` crate (Smithay, MIT)** — links Mesa `libgbm`; needed only on the allocation path for
  hardware that requires the RGB pass (§2).
- Smithay's `DrmCompositor` is **design prior art only** (plane negotiation, TEST_ONLY
  validation, the `queue_frame → vblank → frame_submitted` lifecycle), not a dependency: for one
  fullscreen canvas per connector the right altitude is drm-rs + gbm directly (~1–2k lines), not
  smithay's renderer/element abstractions.

**Sink loop (per ADR-0044), one dedicated thread per card, owning the DRM fd:**

1. Engine tick → publish canvas into the sink's single-slot mailbox (wait-free; triple-buffer
   discipline as in `multiview-framestore`).
2. Sink thread blocks on the DRM event fd. On **page-flip-complete** → take the latest mailbox
   frame → convert/import as the hardware tier requires (§2) →
   `atomic_commit(NONBLOCK | PAGE_FLIP_EVENT)`.
3. **At most one in-flight commit per CRTC is kernel-enforced**: a second `NONBLOCK` commit while
   a flip is pending fails `EBUSY`. That *is* mailbox conflation — we never queue, never retry in
   a loop; the next flip event drains the latest frame. If the mailbox holds nothing new, **do
   nothing**: KMS repeats the current framebuffer for free.
4. **`ALLOW_MODESET` never on the frame path.** Full modesets (mode change, link training) take
   tens of milliseconds; they happen at startup and on explicit Class-2 reconfiguration only
   (invariant #11 — a head modeset blanks *that head* briefly and is surfaced as such; program
   output is unaffected).
5. **`TEST_ONLY` probing at startup** validates the exact plane/format/modifier combination before
   first use — the runtime capability probe that picks the §2 tier per device, never trusting a
   static table.
6. **Fences:** flip timestamps (`PageFlipEvent`) plus `OUT_FENCE_PTR` (a CRTC property returning a
   sync_file fd that signals at presentation) feed the presented-vs-scheduled skew estimator —
   telemetry for the audio servo (§5) and the F3 presentation discipline (§8). drm-rs has no
   first-class `IN_FENCE_FD`/`OUT_FENCE_PTR` helpers, but both are ordinary KMS properties
   reachable through the generic `AtomicModeReq` property path — a **1-day spike**, with syncobjs
   as the verified fallback (ADR-0044 records the spike outcome).

Bounded by construction: ≤1 in-flight commit, ≤3 buffers per head, no allocation on the flip path
(buffers come from a per-head pool created at modeset time).

## 2. Buffer strategy per hardware (verified per display block)

The decisive question per target is **whether the display block scans out NV12 directly**. This
was verified in kernel source, not vendor marketing:

| Display block | NV12 scanout | Path | Copies / passes (node mode) |
|---|---|---|---|
| **Intel Gen9+ (i915/xe)** | **Yes** — NV12 on primary + sprite planes since Skylake (alignment constraints: even coords, min sizes) | decoder dmabuf → `ADDFB2` → plane | **0 copies, 0 render passes** |
| **Raspberry Pi vc4 (Pi 4 HVS5 / Pi 5 HVS6)** | **Yes** — NV12/NV21 native; NV12 + **P030 additionally with the Broadcom SAND128 modifier** (the hardware decoder's native tiling); planes are universal so NV12 goes on any plane | V4L2 decode → NV12/SAND dmabuf → `ADDFB2`(modifier) → plane | **0 copies, 0 render passes** |
| **AMD DCE11 (GCN3 thin clients, e.g. the HP t630 class)** | **No — impossible via DRM on current kernels.** In `dce110_resource.c` the *primary* `plane_cap` has `nv12 = false`; the silicon's dedicated YUV **underlay** pipe is described (`underlay_plane_cap.nv12 = true`) but `amdgpu_dm_plane.c` only grants YUV formats to PRIMARY/universal planes, and DCE has none — the underlay is never exposed as a DRM plane. (DCN/Raven+ exposes NV12 on primaries; DCE never will.) | mandatory **one wgpu NV12→XRGB pass** into a GBM-allocated scanout buffer (`GBM_BO_USE_SCANOUT\|RENDERING`), dmabuf-imported into wgpu | 0 copies, **1 render pass** |
| **NVIDIA (proprietary)** | **No** — primary + cursor planes only, RGB; and per NVIDIA's own driver README (580.76.05) "buffer allocation and submission to DRM KMS using gbm is not currently supported" — the gbm-bo→ADDFB2→flip path does not exist | **wgpu DRM surface only** (`SurfaceTargetUnsafe::Drm`, which uses `VK_EXT_acquire_drm_display`; wgpu PR #7212) — wgpu owns the swapchain, the NV12→RGB pass is the final render | 0 copies, 1 render pass; **tier-2 support** |

Notes that carry weight:

- **The composited canvas always costs one render pass anyway** (compositing *is* a render pass
  that can target the scanout buffer directly), so tier-2 local multiview is "0 extra copies" on
  every target. The zero-pass rows matter for **display-node mode**, where the decoder's dmabuf
  goes straight to glass.
- **Fallback shader cost is bandwidth-dominated** (read 1.5 B/px + write 4 B/px ≈ 5.5 B/px):
  1080p60 ≈ 124 Mpx/s ≈ **0.7 GB/s** — single-digit % of a GCN3 iGPU on shared DDR4; 4K60 ≈
  **2.7 GB/s** — feasible on GCN3 and Pi 5, marginal on Pi 4 (which is dual-4Kp30-class
  regardless). Estimates, not measurements.
- **wgpu import path**: wgpu trunk (2026) added `texture_from_dmabuf_fd` (PR #9412, v28-era);
  `SurfaceTargetUnsafe::Drm` landed earlier (~v25). The exact workspace wgpu pin is decided when
  implementing (ADR-0044 records it). Even without the new API, `wgpu-hal` `texture_from_raw` +
  ~200 lines of `ash` (`VK_EXT_image_drm_format_modifier` + external memory) is the long-proven
  alternative; GBM+EGL (the Kodi/LibreELEC path) is the last-resort fallback if wgpu-on-Vulkan
  misbehaves on a target.
- **No kernel patching.** The AMD DCE11 underlay pipe stays unused; the RGB pass is the product
  path. We do not carry out-of-tree patches for any target.

## 3. HAL/placement — scanout affinity is required, net-new work

The display sink is the **first GPU-resident raw-frame consumer** in the product, and KMS scanout
requires the framebuffer to live on the **connector-owning GPU**. The HAL cannot express that
today (all verified):

- `crates/multiview-hal/src/probe.rs` (`EnvProbe`, `DeviceCaps`) enumerates **render** capability;
  there is no card-node/connector inventory.
- `crates/multiview-hal/src/capability.rs` `Stage` is exactly `{Decode, Composite, Encode}`
  (`Stage::ALL` is a 3-array) — **no scanout stage exists**.
- `crates/multiview-hal/src/select.rs` `PipelineDemand` carries decode/composite/encode load and
  pins (`Pins`), but **no sink locality**: nothing stops `Selection` from placing composite on a
  GPU that owns no connector.
- `crates/multiview-engine/src/placement.rs` `PlacementController` proposes
  `MigrationPlan`s/splits with no notion of a connector-anchored pipeline — it would happily
  migrate composite off the display GPU, forcing a per-frame GPU→host→GPU copy, exactly the
  fragmentation [ADR-0018](../decisions/ADR-0018.md)'s affinity doctrine forbids.

**Proposed work (ADR-0044, slice-sized):**

1. **Probe**: KMS card-node discovery in `probe.rs` — per card: connectors (+ connected state,
   EDID presence), CRTCs, plane format/modifier lists (the §2 tier), and the render-node ↔
   card-node pairing. Read-only ioctls; feature-gated with the rest of `display-kms`.
2. **Select**: a sink-locality constraint in `select.rs` — `PipelineDemand` gains the set of
   scanout connectors the pipeline must reach; candidates not owning them are rejected (a new
   `RejectReason` variant), exactly like an operator `gpu_pin`.
3. **Placement**: `placement.rs` treats a pipeline with a display sink as **affinity-pinned to the
   connector-owning device** — `MigrationPlan` may never move composite off it; load shedding uses
   the invariant-#9 ladder instead.

On single-GPU targets (every display node, the thin-client tier) the constraint is trivially
satisfied — but the type-level machinery must model it, because the multi-GPU host driving a local
monitor is a supported tier-2 deployment and the GPU test server (2× NVIDIA) is exactly the
machine the gate is validated on.

## 4. Config surface — `Output::Display` and the honest wiring scope

`Output` (`crates/multiview-config/src/schema.rs`, internally tagged `#[serde(tag = "kind")]`)
gains a `Display` variant (proposed):

```toml
[[outputs]]
kind = "display"
id = "out-monitor-left"
connector = "DP-1"                  # KMS connector name; "auto" = first connected
mode = "auto"                       # auto = EDID preferred + exact-rational refresh match (§6)
# mode = { width = 1920, height = 1080, refresh = "60000/1001" }   # forced (EDID-less heads)
audio = { enabled = true }          # ELD-gated; per-connector ALSA device (§5)
gpu_pin = { vendor = "amd", stable_id = "0000:00:01.0" }  # optional (`DevicePin`); scanout implies the §3 locality pin anyway
```

**A schema-only edit yields a parseable-but-skipped output** — the variant must ship wired through
every consumer in the same push (repo doctrine: no partial-ship). Verified wiring scope:

- the five exhaustive same-crate `match`es on `Output`: `explicit_id`, `gpu_pin`, `audio`, `label`
  (`schema.rs`) and `validate_outputs` (`crates/multiview-config/src/lib.rs`);
- `build_outputs` in `crates/multiview-cli/src/pipeline.rs` (the variant must construct a runnable
  sink, not fall through);
- the SPA output-kind handling (output forms/list rendering are generated against the OpenAPI
  spec; the new kind lands there first).

Multi-head walls are expressed via the **existing** `WallConfig`/`HeadConfig`
(`crates/multiview-config/src/wall.rs`) with **one `Output::Display` per head** — §7's "one canvas
per head; no spanning" rule. Same-node heads are flipped in a single atomic commit where the
driver allows it (§9, vc4 hazard).

## 5. Audio — ALSA direct to HDMI/DP, ELD-gated, three clocks

No PipeWire/PulseAudio on the data path; the node talks ALSA directly.

- **Device naming**: use the **`hdmi:CARD=…,DEV=…`** ALSA PCM, not raw `hw:` — the `hdmi:` config
  layer sets the IEC958/AES channel-status bits the sink expects. On x86 the HDA codec exposes
  HDMI/DP PCM subdevices (DP audio rides the same HDA pins); on Pi, **vc4-hdmi is its own ALSA
  card per port** and *must* go through the alsa-lib card config — the raw `hw:` device does not
  apply the required setup (the vc4 quirk).
- **ELD gating**: the video driver parses the sink's EDID audio block and publishes the ELD at
  `/proc/asound/cardN/eld#C.P` (channel counts, rates, monitor name). The ELD is **only valid —
  and audio only flows — while the display pipe is lit**, because HDMI audio travels in data
  islands of the video stream. Our sink keeps the CRTC lit by design (frame repeat is free), which
  is exactly what HDMI audio needs. The sink reads the ELD before opening the PCM and re-checks on
  hotplug; **an EDID-less head has no ELD and therefore no audio path** — stated in the UI, not
  silently swallowed.
- **The three-clock problem**: engine tick, pixel/refresh clock, and the HDA/I2S sample clock are
  three independent crystals drifting at ppm levels. Policy: a **bounded audio FIFO (drop-oldest,
  invariant #10)** feeding the PCM, plus a **buffer-level servo driving an adaptive resampler** so
  the audio rate tracks the *video scanout* clock — the mpv/Kodi "display-resample" technique.
  Measurement: `snd_pcm_status` (avail/delay + audio htstamp) against the §1 flip timestamps. The
  resampler is not new machinery: `multiview-audio` already decodes/resamples every source to the
  canonical 48 kHz float format (`crates/multiview-audio/src/decode.rs`,
  [ADR-R005](../decisions/ADR-R005.md)); the servo varies that resample ratio within a clamped
  ±ppm band. Insert/drop-sample is the crude fallback only.
- **Latency sizing**: 48 kHz S16/S24, period 256–512 frames, 3–4 periods → 16–43 ms device buffer;
  display latency is 1–2 vsyncs anyway, so **AV alignment, not raw latency, is the goal** — the
  servo aligns audio to the flip clock, and the §8 link offset already budgets the whole chain.
- Typical monitors expose **stereo LPCM only** (the measured t630-class ELD: 2-ch LPCM,
  32/44.1/48 kHz); multichannel is whatever the ELD declares, never assumed.

## 6. EDID and mode policy

- **Default: EDID preferred mode.** Parse EDID via **libdisplay-info** (freedesktop C library;
  Smithay maintains MIT Rust bindings) — never hand-rolled parsing.
- **Exact-rational refresh match**: among acceptable modes, pick the one whose refresh as an
  **exact rational** matches the engine cadence (59.94 Hz = 60000/1001 modes exist in EDID; never
  compare float fps — invariant #3). A matched mode reduces §7 repeat/drop to zero in steady
  state.
- **CVT-RB forced-mode fallback for EDID-less heads — a verified field requirement, not
  nice-to-have.** The HP t630 test unit has one display chain that forwards **no EDID at all**
  (0 bytes; the head runs today off a `video=DP-1:1920x1080M@50` kernel-cmdline forced mode, scanning
  a CVT-RB-style 1080p at 49.98 Hz). The product handles this *without* kernel cmdline surgery:
  the per-connector `mode = { width, height, refresh }` config override computes a CVT-RB timing
  and commits it directly. EDID-less heads get video only (no ELD → no audio, §5).
- **Hotplug**: udev "change" uevent on the card device, debounced (connectors flap during link
  training); container caveats in §10. Re-probe → re-validate with `TEST_ONLY` → modeset is
  Class-2 on that head only.

## 7. Vsync vs tick — reconciliation, and the one deliberate exception

Two free-running clocks meet at the flip: the engine cadence (invariant #1) and the display
refresh. The default policy is **repeat/drop at the mailbox** (§1): at 60.000 Hz tick vs 59.94 Hz
display, that is one duplicated-or-dropped frame every ~16.7 s — a single duped frame, visually
negligible, and §6's exact-rational mode selection usually eliminates it entirely.

**Optional display-locked clock mode (node-only, Class-2, never the default).** For a *dedicated
display node*, the panel **is** the product output, and a clock that free-runs against it
permanently dupes/drops. The option derives the node's output cadence *from vblank timestamps*
(the mpv display-resample analogue): tick deadlines slew, within a clamped ppm band, to the
measured refresh. The invariant-#1 argument, spelled out (ADR-0045 carries the normative text;
ADR-0044 defers this mode to it):

- Invariant #1 forbids **inputs, clients, and sinks pacing the program output clock**. On a
  display node there is no downstream program — the panel is the terminal output device, exactly
  the role an SDI output card's clock plays in a broadcast chain. Locking the node's local
  presentation cadence to its own glass lets **no input and no client** pace anything; ingest
  stays sampled through the framestore ladder unchanged.
- It is **never available on a tier-2 local multiview**: there the engine's output clock serves
  encoders and network outputs too, and a monitor must never pace those (that *would* break #1).
  Config validation rejects the combination.
- It is **Class-2** (a clock-domain change; outputs on that node restart) and **off by default**;
  the default repeat/drop policy is always correct, merely occasionally non-ideal.

## 8. Node presentation model (the F3 seam)

How a display node decides **which frame** goes on **which vblank** — the consumer half of
[ADR-M010](../decisions/ADR-M010.md)'s outbound epoch (the producer half — the engine emitting one
outbound `WallClockRef` per program over the control WS, RTCP SR on RTSP, `EXT-X-PROGRAM-DATE-TIME`
on HLS — amends [wall-clock-sync](wall-clock-sync.md); none of it exists in `multiview-output`
today, verified).

- **Inputs to the chooser**: the program epoch (`WallClockRef`-shaped affine map output-PTS ↔
  shared wall ns; `multiview-core/src/wallclock.rs` is the existing exact-affine type — i128
  intermediates, never float), a per-deployment **link offset**, and the node's disciplined local
  clock (ptp4l preferred; chrony fallback).
- **The algorithm**: decode into a small queue (2–3 frames). At each flip decision, pick the frame
  whose `wall_at(pts) + link_offset` is closest to the **predicted next vblank** (KMS gives
  precise vblank timestamps + sequence counters). Repeat the current frame if the next frame's
  deadline is still in the future; drop if behind. **Discipline is pure frame choice** — nodes are
  pull-side consumers; nothing ever feeds back to the engine (invariants #1/#10 safe by
  construction).
- **Link offset** is AES67's semantics applied to video: a fixed receiver-side delay (default ≈ 2×
  max network jitter + decode time, typically 100–300 ms). **Uniformity across nodes is what
  matters, not smallness** — every node configured with the same offset presents the same frame at
  the same instant.
- **Clock layer**: ptp4l with software timestamping measures ±5–50 µs typical on a quiet GbE LAN
  (spec the tier as ±100 µs guaranteed); hardware timestamping is sub-µs (Pi 5 has a HW-PTP-capable
  NIC; Pi 4 and the t630-class NIC are assumed software-only pending `ethtool -T` per unit).
  chrony lands ~0.5–1 ms — still ~1/30 of a 60 Hz frame. Either is far below the half-frame
  decision threshold.
- **Degradation**: a node that loses the control WS keeps the last epoch and free-runs on its
  disciplined clock — drift-bounded, output never falters; on rejoin it re-converges the offset
  *before* unmuting any visible correction. Audio follows the §5 servo, never its own clock.

## 9. Display-node mode (`multiview node`)

A subcommand of the existing `multiview` binary, not a second binary. The pipeline **reuses the
ingest stack unchanged**: `multiview-input` pacer/jitter/normalize/supervised-reconnect → the
framestore tile ladder (`TileStore::read_at`, LIVE→STALE→RECONNECTING→NO_SIGNAL) → a single-source
full-canvas composite → the display sink. The node thereby inherits the product's resilience
doctrine **for free**: a broken feed rides last-good, then the local slate
(`multiview-engine/src/slate.rs` builds slates once, never per-tick) — a display node never shows
garbage and never goes black while it has power.

- **Stream from the central Multiview**: any of RTSP/SRT/HLS; codec chosen against the node's
  decode matrix (§11/§12 — H.264 or HEVC-8b for the thin-client class; **HEVC for Pi 5**, which
  has no H.264 hardware decoder).
- **Dual-head nodes**: one sink per head (one canvas per head — no spanning; spanning couples two
  refresh domains and amplifies the vc4 cross-CRTC hazard below). Where both heads change in one
  tick, submit **one atomic commit covering both CRTCs**: vc4 keeps driver-global state that
  serializes commits across CRTCs (rpi/linux issue #5094), so two separate commits can slip a
  frame on Pi dual-head — budget for occasional slips regardless.
- **Enrollment, pairing, assignment, fleet management** are the Devices domain's job — driver
  `displaynode` in [managed-devices](managed-devices.md) (keypair-bound enrollment tokens, screen
  pairing, wall-head binding, IPv6-first control surfaces per
  [conventions §10](../architecture/conventions.md) / [ADR-0042](../decisions/ADR-0042.md)). This
  brief deliberately stops at the media path.

## 10. Deployment

**Bare-metal systemd is the blessed path** (minimal Debian or Raspberry Pi OS Lite):

- No Xorg, no Wayland, no display manager, no seatd/logind. A process that opens
  `/dev/dri/cardN` when no other master exists becomes **DRM master implicitly** — no VT, no
  `VT_SETMODE`. fbcon is an in-kernel client and does not hold master; on crash the kernel's
  fbdev-client restore brings the console back (no stuck black screen).
- Unit outline: `After=`/`BindsTo=dev-dri-card0.device`, `User=multiview`,
  `SupplementaryGroups=video render audio`, `Restart=always`, `WatchdogSec=` + sd_notify;
  `systemctl mask getty@tty1`; kernel cmdline cosmetics
  (`quiet loglevel=3 consoleblank=0 vt.global_cursor_default=0`).
- Packages: kernel + firmware (amdgpu needs `firmware-amd-graphics`), `libgbm1`,
  `mesa-vulkan-drivers` (for the wgpu paths), ALSA libs. Boot-to-first-frame estimate **6–15 s**
  (LibreELEC demonstrates ~10 s-class boots on the same hardware; estimate, not measured).
- Incumbent desktop stacks are **disabled, not coexisted with** — there is one DRM master per
  card; running as an X11 client or via DRM leases from a compositor is explicitly out of scope
  (§15). Field reality: the HP t630 test unit boots an incumbent X11 signage stack; the unit is
  approved as a dedicated display-node target, so the takeover (disable the display manager,
  become DRM master) is unblocked (§14).

**Container is genuinely workable (supported-but-secondary):**

- A **rootful** container with `--device /dev/dri --device /dev/snd` (+ matching group GIDs) can
  take DRM master — **no extra capabilities needed**: first open of the primary node becomes
  master implicitly when no other master exists. A live master can **never** be displaced —
  `SET_MASTER` fails `EBUSY` regardless of capabilities; CAP_SYS_ADMIN is needed only to *acquire*
  master via `SET_MASTER` on an fd that was never master (the vacant-slot case). The host must
  boot with no KMS client of its own.
- **Hotplug, corrected finding**: kernel kobject uevents **are** delivered to a container's own
  network namespace when that netns is owned by the initial user namespace — i.e. a normal rootful
  container **does receive kernel hotplug uevents**. The KMS backend therefore listens on the
  **kernel netlink uevent group directly** (not the udevd-processed stream, which genuinely does
  not reach the container). Design rule, not deployment advice.
- **Rootless containers** (user-ns-owned netns) get no kernel uevents → fall back to polling
  `get_connector(force_probe = true)` at 2–5 s (the kernel itself polls non-HPD connectors at
  10 s). EDID stays readable via `/sys/class/drm/<conn>/edid` in both cases.
- ALSA passthrough is plain `/dev/snd` — no Pulse/PipeWire socket games, since we are ALSA-direct.

## 11. Raspberry Pi 4/5 specifics (Pi 4 being provisioned; Pi 5 aspirational)

A Raspberry Pi 4 is being provisioned for the validation fleet; no Pi 5 exists today. Everything
here stays research-grade until §14's hardware validation runs, and the brief says so plainly.

| | Pi 4 (BCM2711) | Pi 5 (BCM2712) |
|---|---|---|
| H.264 decode | **HW**, up to 1080p60 (V4L2 stateful) | **NONE — no H.264 hardware decoder.** CPU-only (~4–8× 1080p25-class sw decodes at high load); prefer HEVC streams to Pi 5 nodes |
| HEVC decode | HW, up to 4Kp60 (v4l2-request) | HW, 4Kp60 (rpi-hevc-dec) |
| Display | 2× micro-HDMI: single 4Kp60 **or** dual 4Kp30 | dual 4Kp60; HDR output supported |
| NV12 scanout | native (+ P030/SAND128) | native (+ P030/SAND128) |
| Vulkan (v3dv) | 1.2 conformant (1.3 with Mesa 24.3) | 1.2/1.3 conformant |
| PTP | software timestamps only | **hardware PTP NIC** |

- **Decode budgets** (engineering estimates against fixed-throughput decoders): Pi 4 H.264 block ≈
  124 Mpx/s aggregate (~4× 720p25 or ~9× 480p25 tiles); HEVC block ≈ 500 Mpx/s (~16× 576p25
  tiles). Decode-at-display-resolution (invariant #6) is what makes Pi-class tier-2 multiview
  plausible at all; the **display-node mode is the Pi sweet spot** — V4L2 decode → NV12/SAND
  dmabuf → plane: zero copies, near-zero 3D-GPU use, audio via the per-port vc4-hdmi ALSA cards.
- **Known risks, all gated by hardware validation**: wgpu-on-v3dv has field-reported perf cliffs
  (works-but-validate; our shader set — sample + matrix + blend — is the well-trodden path);
  `VK_EXT_image_drm_format_modifier` on v3dv is **unconfirmed** — if absent, the wgpu dmabuf
  import needs linear buffers or falls back to `Surface::Drm`/GBM-EGL; **NV12 direct-scanout
  regressions have shipped in some Pi kernel windows** (rpi/linux issue #5727: solid-green
  output) — hence the golden NV12-scanout gate on the deploy kernel (§13); the dual-head
  cross-CRTC commit-serialization hazard (#5094, §9).

## 12. The thin-client capability envelope (measured on the HP t630 test unit)

Measured live (2026-06-10, read-only probe) — this is the floor we design the thin-client tier
against, not a guess:

- **GPU**: AMD GCN3 ("Carrizo/Stoney"-class embedded APU, **DCE11 display block**), amdgpu with DC
  atomic; Mesa 25.2.8; **RADV Vulkan present** (wgpu viable); 1 GiB VRAM carve-out from 8 GB
  DDR4-1866 dual-channel (~29.8 GB/s shared CPU+GPU).
- **VA-API decode**: H.264 ConstrainedBaseline/Main/High, **HEVC Main 8-bit**, MPEG2, VC-1, JPEG.
  **Encode: H.264 only** (VCE 3.x). **No HEVC 10-bit / VP9 / AV1 decode; no HEVC/AV1 encode.**
  VideoProc (vpp) entrypoint present — a VAAPI CSC/scale alternative to the shader pass if ever
  needed.
- **KMS planes: 6 planes for 3 CRTCs — 3 primary + 3 cursor, no overlays, all RGB-only** (primaries:
  18 formats, 8/10/16-bit RGB incl. XR24/XR30/FP16; cursors AR24 only). Confirms §2: the NV12→XRGB
  pass is mandatory on this class.
- **Heads**: 2× DisplayPort. One chain is **EDID-less** (forced CVT-RB 1080p@49.98 via kernel
  cmdline today — the §6 forced-mode requirement, proven in the field); the other is a 1080p60
  monitor attached through a DP→HDMI adapter (its EDID carries an HDMI VSDB) with a valid ELD:
  **stereo LPCM, 32/44.1/48 kHz**. The two heads free-run at **different refresh rates today
  (49.98 vs 60.00 Hz)** — direct evidence for §13's "same-node heads are not phase-locked".
- **CPU**: 2 modules / 4 hardware threads (Excavator-class) at 0.9–2.0 GHz — the binding
  constraint for many-input demux/network/audio, not for the display path.
- **Decode ceiling is unbenchmarked**: the UVD6 aggregate sits somewhere in the **250–500 Mpx/s
  band** (the vendor publishes no fps ceiling). Tier-1 display node (1× 1080p60 ≈ 124 Mpx/s) has
  2–4× headroom — unequivocal yes. Tier-2 local multiview saturates at roughly **4–8× 1080p30
  tiles**; **9× 1080p30 (≈560 Mpx/s) exceeds the band at any plausible value** and needs
  720p-class renditions and/or the invariant-#9 degradation ladder. The on-unit benchmark that
  settles the band is a scheduled validation item (§14) **before** committing published tile-count
  tiers.

## 13. Sync tiers — what is promised, and what is never promised

The published table (normative in [ADR-M010](../decisions/ADR-M010.md); device tiers C/D belong to
[managed-devices](managed-devices.md)):

| Tier | Output class | Achieved |
|---|---|---|
| **S** | Same node, multi-head | **Frame-accurate** (one atomic commit where the driver allows); sub-ms flip delta typical — but CRTCs still free-run (the t630 test unit's two heads run 49.98 vs 60.00 Hz today) — identical modes required for best results |
| **A** | Display nodes + PTP | **Frame-accurate** (same frame index everywhere); residual 0–1 refresh (0–16.7 ms @60 Hz) of vsync phase |
| **B** | Display nodes + chrony only | Frame-accurate with occasional ±1-frame decisions at boundaries |
| **C** | Vendor HDMI decoder appliances | Bounded drift, ±100–500 ms — see [managed-devices](managed-devices.md) |
| **D** | Cast-class consumer endpoints | None / seconds — never part of a synchronized canvas |

**Never promised, stated plainly:**

- **No scanout-phase (genlock-grade) alignment.** Each display's refresh derives from a
  free-running local crystal; phases drift through each other. Frame N's scanout start on two
  nodes differs by a **uniform 0–16.7 ms residual @60 Hz (mean ~8.3 ms)** — invisible on
  physically separated monitors, disqualifying for zero-bezel tiled walls with motion crossing the
  seam. True framelock needs dedicated sync hardware; the lone commodity exception (Pi ≤3's
  trimmable pixel clock) is gone on Pi 4/5. UI badges and docs must carry the residual, not bury
  it.
- **No engine pacing from any node or external clock** — PTP disciplines a reference estimate
  ([wall-clock-sync](wall-clock-sync.md); the `ptp.rs` servo's own doc pins this); nodes never
  feed back; §7's display-locked option is node-local and Class-2.
- **Multi-node *audible* audio defaults to a single-audio-node policy** (two free-running rooms of
  program audio = comb filtering → echo); disciplined multi-node audio is opt-in on our nodes
  only.

## 14. Validation plan

1. **TEST_ONLY runtime probe** (every start): the exact plane/format/modifier set is validated
   before first commit; the §2 tier is *chosen by the probe*, never assumed.
2. **Golden NV12-scanout gate** on the deploy kernel for NV12-direct targets (Pi class): scan out
   a known NV12 pattern, capture (HDMI-USB dongle or readback), compare — catches #5727-class
   kernel regressions in CI/HW before the fleet does.
3. **Invariant-#10 chaos gate**: SIGSTOP/hang/kill the display sink thread mid-run; assert zero
   engine tick overruns and unchanged encode fan-out. The mailbox publish must be provably
   wait-free (extends the existing preview-tap chaos discipline).
4. **Frame-accuracy acceptance (with ADR-M010)**: burnt-in frame-index counter + binary flash;
   photograph all displays at ≤1/1000 s exposure; automate with a USB camera + OCR (≥99% identical
   counters over N=100 samples, max delta ≤1). Phase layer: photodiode per display into a
   2-channel scope/line-in (50 µs class) to confirm the residual is bounded by one refresh.
5. **24 h soak**: two nodes (one thin-client-class + one Pi once acquired) on a non-PTP GbE
   switch, software-timestamp ptp4l; pass = clock 99th-pct |offset| ≤100 µs (PTP) / ≤1 ms
   (chrony), frame-accuracy per (4), zero output-cadence deviations. **Chaos extension**: kill
   PTP and the control WS mid-soak — output cadence unaffected; nodes degrade to free-run.
6. **The decode-throughput benchmark on the HP t630 test unit** — settles the 250–500 Mpx/s UVD6
   band and the published tier-2 tile counts. The unit is approved as a dedicated test target
   (takeover = disable the incumbent X11 signage stack's display manager, become DRM master).
7. **NVIDIA smoke on the GPU test server**: wgpu DRM-surface display-out (tier-2 path) + the §3
   multi-GPU scanout-affinity gate (placement must refuse to migrate composite off the
   connector-owning GPU).

## 15. Non-goals

- **No genlock / scanout-phase alignment / zero-bezel wall seam guarantees** (§13).
- **No NV12 scanout on AMD DCE11** — the kernel never exposes it; we do not patch kernels.
- **No raw-KMS (GBM→ADDFB2) path on NVIDIA** — vendor-documented unsupported; wgpu DRM-surface
  only, tier-2.
- **No X11/Wayland client mode and no DRM leases from a running compositor** — the node owns DRM
  master, full stop. Incumbent desktop stacks are disabled, not coexisted with.
- **No canvas spanning across CRTCs** — one canvas per head; walls are per-head sinks over
  `WallConfig`.
- **No Windows** (repo non-goal).

## Decision records

- [ADR-0044](../decisions/ADR-0044.md) — local display output via DRM/KMS: the `display-kms`
  sink, mailbox flip policy, per-hardware buffer strategy, scanout affinity in HAL/placement.
- [ADR-0045](../decisions/ADR-0045.md) — display-node mode: `multiview node`, enrollment/pairing
  (with [managed-devices](managed-devices.md)), node presentation discipline, the display-locked
  clock option and its invariant-#1 boundary.
- [ADR-M010](../decisions/ADR-M010.md) — multi-output timing & sync: the outbound `WallClockRef`
  epoch, link-offset semantics, sync groups, and the published tier table (§13).
