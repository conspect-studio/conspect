# NDI + NDI|HX integration — research brief

**Status:** research complete (2026-06-06). Synthesises two fan-outs:
`wzkbfis04` (practical encode/decode with the SDK + the FFI ABI) and
`w5a9indwl` (SDK-free reverse-engineering + SpeedHQ + legal). Drives the
`NDI-L*` plan and refines [ADR-0008](../decisions/ADR-0008.md).

> **NDI is a NewTek / Vizrt product (Vizrt NDI AB).** Not Audinate (that's Dante).
> The "SDK" is the NewTek/Vizrt NDI SDK — free-to-use, proprietary EULA,
> runtime-loaded via `NDIlib_v6_load`, never vendored (ADR-0008). The operator
> has a **trial SDK licence**.

---

## 0. Headlines that reshape the plan

1. **NV12 is a first-class NDI FourCC on SEND** (`NDIlib_FourCC_video_type_NV12`).
   Our internal NV12 (inv #5) hands **directly** to NDI send — *zero* pixel
   conversion. (Verify on the trial build that High-Bandwidth send accepts NV12
   rather than silently re-encoding via UYVY — the one thing unconfirmed from a
   primary spec sentence.) On **RECEIVE** there is **no NV12 color_format** — the
   YUV recv option yields **UYVY** (4:2:2), which we already convert to NV12.
2. **HX *decode* is FREE via the standard SDK.** A normal receiver connected to an
   **NDI|HX / HX2 / HX3** source gets **decoded, uncompressed `NDIlib_video_frame_v2_t`
   frames** exactly like Full NDI — the SDK decodes H.264/HEVC transparently. So
   "receive NDI" (any variant) needs no codec work from us.
3. **HX *encode/send* needs the NDI *Advanced* SDK** (separate licence + a vendor
   ID): you create with `NDIlib_send_create_v2` and submit `NDIlib_compressed_packet`s
   carrying your **own** H.264 (HX) / HEVC (HX2/HX3) bitstream — which we already
   produce (NVENC / x264/5). The **standard** SDK only does uncompressed frames.
4. **`clock_video=false`, `clock_audio=false` on send** — when `true`, the send
   call *rate-limits/blocks* to the frame's declared rate, which would collide with
   the output clock (inv #1). NDI must be a pure sink paced by **our** tick.
5. **Async-send buffer-lifetime contract** (Vizrt: "the single most common
   problem"): after `NDIlib_send_send_video_async_v2`, the SDK reads the buffer
   until the **next** async-send or destroy → **double-buffer** (pin frame N until
   N+1 is submitted; this fits our per-device frame pool + `Drop` discipline).
6. **Full-NDI *decode* is open even without the SDK** — FFmpeg has a reverse-
   engineered **SpeedHQ** decoder since 2017 (`libavcodec/speedhq.c`), and
   VideoLAN's **libndi** (JB Kempf, C, **LGPLv2.1**) discovers + receives Full NDI
   (no send, no HX). This enables an **open, default-build, receive-only** NDI path
   (§5, path B).

## 1. Two paths (not mutually exclusive) — the decision

| | A. Official SDK (trial-licensed) | B. Open `libndi` + FFmpeg SpeedHQ |
|---|---|---|
| Receive Full NDI | ✅ (uncompressed frames) | ✅ (libndi discover/recv + SpeedHQ decode) |
| Receive HX/HX2/HX3 | ✅ (transparent decode) | ❌ (libndi has no HX) |
| **Send** Full NDI | ✅ (NV12 native) | ❌ (no open SpeedHQ encoder) |
| **Send/recv HX** | ✅ recv; send = **Advanced SDK** | ❌ |
| Licence/build | proprietary, **operator-gated, never-vendored** (`ndi` feature off by default) | **LGPL** — can ship in the **default** build |
| Discovery, tally, metadata, groups | ✅ full | partial (discovery yes) |

**Recommendation:** do **both**, sequenced. **A is the priority** (we have the
trial; it's the complete feature set — send + all-variant recv + tally/metadata —
behind the existing `NdiLicense` gate). **B is a high-value follow-on**: open,
LGPL-clean, default-build **NDI ingest for everyone** (Full NDI receive) with the
proprietary SDK reserved for send + HX. New plan item `NDI-RECV-OPEN`.

## 2. Send (encode) — standard SDK

`NDIlib_send_create({ p_ndi_name, p_groups, clock_video:false, clock_audio:false })`
→ `NDIlib_send_send_video_async_v2(inst, &frame)` (async, pin the buffer) or
`_v2` (sync, buffer free on return) → `NDIlib_send_destroy`.

`NDIlib_video_frame_v2_t`: `xres, yres, FourCC (NV12), frame_rate_N/D,
picture_aspect_ratio, frame_format_type (progressive), timecode (or the
`NDIlib_send_timecode_synthesize` sentinel), p_data, line_stride_in_bytes,
p_metadata, timestamp`. Timecode/timestamp re-stamped from our tick (inv #3).
Audio: `NDIlib_send_send_audio_v2/v3` with planar float from the program bus.

## 3. Receive (decode) — standard SDK

`NDIlib_recv_create_v3({ source_to_connect_to, color_format (prefer a YUV→UYVY),
bandwidth (highest), allow_video_fields:false })` →
`NDIlib_recv_capture_v2/v3(inst, &video, &audio, &meta, timeout)` returns a
`NDIlib_frame_type_e` (video/audio/metadata/none/error); free each with
`NDIlib_recv_free_video_v2` etc. HX sources decode transparently to uncompressed
frames. Recv UYVY → our NV12 via the existing converter. Sampled, non-blocking,
into the per-source `AudioStore`/`TileStore` (inv #1/#10) — exactly the
`SdkNdiReceiver` shape behind the existing `NdiReceiver` trait.

## 4. Find / HX / Advanced

- **Find:** `NDIlib_find_create_v2({show_local_sources, p_groups, p_extra_ips})` +
  `find_get_current_sources` (+ `find_wait_for_sources`). The source array is owned
  by the finder and invalidated on the next call → **copy** `p_ndi_name` /
  `p_url_address` into owned `CString`s.
- **HX send (Advanced SDK):** `NDIlib_send_create_v2` + submit
  `NDIlib_compressed_packet` (fourcc `H264`/`HEVC`, pts/dts, flags, data + extradata)
  carrying our NVENC/x264 bitstream. Needs the Advanced SDK + a vendor ID — confirm
  whether the trial covers Advanced.

## 5. FFI ABI plan (drives NDI-L1)

`NDIlib_v6_load` returns a pointer to a **versioned struct of function pointers**
(field order is load-bearing; v6 appends to v5/v4…). The sound, low-risk approach
(also what mature bindings converge on):

- **`bindgen` the licensed header** (`/opt/ndi/include/Processing.NDI.*.h`, present
  once `NDI-L0` installs the SDK) **at build time, gated behind the `ndi` feature**,
  to generate the `NDIlib_v6` function-table struct + the data structs
  (`video_frame_v2_t`, `audio_frame_v2/v3_t`, `source_t`, `send_create_t`,
  `recv_create_v3_t`, `tally_t`, `metadata_frame_t`, `find_create_t`). This reads the
  **real** header — no guessing struct offsets (avoids the UB risk that blocked us).
- Keep the existing **runtime-load** (`NDIlib_v6_load` via `libloading`) in
  `multiview-ndi-sys`; cast the returned table to the bindgen'd `*const NDIlib_v6`
  and call through it. All `unsafe` + `// SAFETY:` stays in `multiview-ndi-sys`;
  `multiview-output`/`-input` implement `NdiApi`/`NdiReceiver` over it and stay
  `forbid(unsafe)`. ADR-0008 honoured: header is build-time-only; the `.so` is
  dlopen'd; nothing vendored.
- ABI gotchas to verify against the header: enum sizes, `bool` width, the
  `source_t` `p_url_address`/`p_ip_address` union (bind the single pointer slot),
  struct padding, the timecode-synthesize sentinel, 32- vs 64-bit.

## 6. Updated plan

- `NDI-L0` (devcontainer SDK, gated) → `NDI-L1` becomes: **bindgen the v6 header
  behind `ndi`** + `SdkNdiApi` (send) + `SdkNdiReceiver` (recv) over the runtime
  table; loopback NV12→NDI→NV12 (clock off; async double-buffer). → `NDI-L2/L3`
  egress/ingest wiring (NV12 native send; UYVY recv) → `NDI-L4` find → `NDI-L5`
  audio → `NDI-L6` tally/metadata → `NDI-HX` (Advanced SDK: feed our H.264/HEVC).
- **NEW `NDI-RECV-OPEN`** `L` — open, LGPL, **default-build** Full-NDI **receive**
  via `libndi` (LGPLv2.1) + FFmpeg SpeedHQ, no proprietary SDK. Discovery + recv
  only; HX/send stay SDK-gated. (Evaluate: link libndi vs port the receive path.)

## Citations

- NDI SDK docs — <https://docs.ndi.video/> (Send/Recv/Find references; clock + async-buffer contract).
- NV12 FourCC + native send — public `Processing.NDI.structs.h`; DistroAV/OBS-NDI notes.
- FFmpeg SpeedHQ decoder — `libavcodec/speedhq.c` (2017); <https://wiki.multimedia.cx/index.php/SpeedHQ>.
- VideoLAN libndi — <https://code.videolan.org/jbk/libndi> (LGPLv2.1; IBC 2025).
- Full research transcripts: workflow tasks `wzkbfis04`, `w5a9indwl`.
