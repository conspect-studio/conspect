# Research brief — Codec capabilities: a typed catalog, the transport×codec matrix, and the codec×acceleration class

- **Status:** Draft (verification-hardened)
- **Area:** Config / FFmpeg / HAL / Output / Control / Web
- **Date:** 2026-06-08
- **Decision:** [ADR-0036](../decisions/ADR-0036.md)
- **Builds on / grounds in:**
  - [ADR-0035](../decisions/ADR-0035.md) + [self-aware-placement.md](self-aware-placement.md) — runtime capability detection; the `CapabilityReport` keystone (the *"what the hardware can actually do"* half).
  - [ADR-M007](../decisions/ADR-M007.md) — `CapabilityReport` as the **single** machine-readable gate for UI **and** validator (never two sources of truth).
  - [ADR-0003](../decisions/ADR-0003.md) — three-layer capability model; *"NVML cannot report codec capability — must use SDK caps APIs or probes."*
  - [ADR-0011 / ADR-0012](../decisions/README.md) + [conventions.md §4/§7](../architecture/conventions.md) — feature flags and LGPL-default licensing discipline (`gpl-codecs` off by default; `ndi` proprietary runtime-loaded).
  - [ADR-0026](../decisions/ADR-0026.md) — encode-once-mux-many (decodable ≠ encodable; two codec axes).
  - [management-capability-matrix.md](management-capability-matrix.md) — a codec change on a live output is a **Class-2** controlled reset.
  - **Precedent to mirror:** `crates/multiview-audio/src/capability.rs` — the audio transport-carriage model (`OutputTransport` + pure-const `for_transport()` + typed `validate_*`).

> This brief explains **why** the codec selection surface is rebuilt. The *how* (the
> dependency-ordered backlog) is in [ADR-0036](../decisions/ADR-0036.md). It does **not**
> duplicate ADR-0035's probe design or the licensing ADRs — it composes with them.

---

## 1. The problem (verified in code, 2026-06-08)

The output `codec` is **free text** with effectively no validation, and the run-time path
**silently substitutes MPEG-2** for anything it cannot encode. This is how the demo
silently became `mpeg2video`: the operator typed a codec the build could not encode, and
nothing rejected it.

Three verified facts, end to end:

1. **The config field is `pub codec: String`** on 5 of the 6 `Output` variants —
   `crates/multiview-config/src/schema.rs`: `RtspServer`, `LlHls`, `Hls`, `Rtmp`, `Srt`
   each carry a free-text `codec: String` (doc-commented *"Video codec (`h264`, `hevc`, …)"*).
   `Output::Ndi` correctly carries **no** codec field (NDI's wire codec is intrinsic to the
   transport). There is exactly one `pub enum Output`; no shadow/typed variant hides a
   constrained codec.

   > Citation note: per-field doc-comments have shifted line numbers since the original
   > survey. Anchor on **symbol names**, not absolute lines — the `codec: String` fields
   > and the `Output::Ndi` variant are the stable references.

2. **The only validation is non-empty.** `validate_outputs()` in
   `crates/multiview-config/src/lib.rs` destructures exactly the five codec-bearing variants
   into `Some(codec)` (and `Ndi` into `None`), and the *only* check applied is
   `if codec.is_empty()` → `ConfigError::Validation("an output declares an empty codec")`.
   A workspace-wide grep for codec-token literals (`h264`/`hevc`/`av1`/…) inside
   `crates/multiview-config/src` returns **zero** hits: there is no transport-carriage matrix,
   no real-codec allowlist, no host check anywhere in the config crate. Any non-empty
   string — `"not_a_codec"`, `"h265"` on classic RTMP, `"av1_nvenc"` on a non-Ada GPU —
   passes config validation untouched.

3. **Two silent `mpeg2video` substitutions in `crates/multiview-cli/src/pipeline.rs`:**
   - `logical_codec(token)` matches known tokens (`h264`/`avc`→H264, `h265`/`hevc`→H265,
     `ffv1`→Ffv1, `mjpeg`→Mjpeg) and `_ => VideoCodec::Mpeg2Video` maps **every unknown
     token** (a typo like `h.264`, or `av1`/`vp9`, which have no encode variant) to MPEG-2.
   - `resolve_encoder()` tries `select_encoder(requested)`; if `None`, it emits a
     `tracing::warn!` and falls back to `select_encoder(VideoCodec::Mpeg2Video)`.
     `PipelineError::Codec` is returned **only** if even the MPEG-2 fallback is absent —
     so the requested-codec failure **never surfaces to the operator as an error**; it is
     logged (on a daemon, into logs nobody reads) and swapped.

   **Net effect, traced for the common case:** on the LGPL-clean default build (no `cuda`,
   no `gpl-codecs`), a user who types the valid token `"h264"` gets `logical_codec → H264`,
   then `candidate_encoders(H264)` is **empty** (no NVENC without `cuda`, no `libx264`
   without `gpl-codecs`, and H.264/H.265 have **no** LGPL software encoder —
   `lgpl_software_encoder()` returns `None`), so `select_encoder(H264)` is `None`, the
   warn+fallback fires, and the output is silently `mpeg2video`. Confirmed by the in-tree
   test `default_build_cannot_encode_h264_or_h265` (asserts `candidate_encoders(H264)` is
   empty and `!can_encode(H264)` under `cfg(not(gpl-codecs))` + `cfg(not(cuda))`).

The product goal — most robust, bulletproof, **really easy to use** — demands the inverse:
the form offers **only valid choices**, each labelled with its acceleration class, and a
codec that cannot be carried or encoded is **rejected with an actionable reason**, never
silently downgraded.

---

## 2. As-built building blocks (what already exists to reuse)

| Building block | Where | What it gives us |
|---|---|---|
| Typed encode enums (no metadata, wrong crate) | `multiview-ffmpeg/src/codec.rs` — `VideoCodec { H264, H265, Mpeg2Video, Ffv1, Mjpeg }`, `AudioCodec { Aac, Opus, Mp2 }` | The variant skeleton + the per-(codec, backend) encoder **resolvers**. `#[derive]`s `Debug/Clone/Copy/PartialEq/Eq/Hash` only — **not** serde; **no** license/wire metadata; **no** AV1/VP9/VP8. |
| Encoder resolvers (honest, feature-gated) | same file — `lgpl_software_encoder()` (H264/H265 → `None`), `gpl_software_encoder()` (`cfg(gpl-codecs)` → `libx264`/`libx265`), `nvenc_encoder()` (`cfg(cuda)` → `h264_nvenc`/`hevc_nvenc`), `candidate_encoders()`, `select_encoder()` | The **truth of license-by-backend** is already encoded here; the catalog must surface it, not re-derive it. |
| Native-AAC discipline | same file — `AudioCodec::Aac → "aac"` (native LGPL), test `nonfree_libfdk_aac_is_never_a_candidate` | `libfdk_aac` (nonfree) is **never** a candidate in any build. Catalog must not introduce a nonfree audio variant. |
| Decode enum (broader than encode) | `multiview-ffmpeg/src/hwdecode.rs` — `HwInputCodec { H264, H265, Av1, Vp9, Mpeg2Video }` + `*_cuvid` mapping | AV1/VP9 are **decodable** but not yet **encodable** — confirms the two-axis split (ADR-0026). |
| RTSP transport gate (to generalize) | `multiview-output/src/rtsp_server/caps.rs` — `RtspCodec { H264, H265 }`, `from_codec_name()`, `RtspCapsError::UnsupportedCodec` (*"codec `{x}` is not RTSP-payloadable (only h264/h265)"*) | A concrete, pure, compile-time per-transport gate with a typed refusal — the exact shape to generalize per-transport. |
| Audio transport-carriage precedent | `multiview-audio/src/capability.rs` — `OutputTransport { MpegTs, Rtsp, Hls, Rtmp, Ndi }`, pure-const `for_transport()`, `validate_tracks()`/`validate_channel_map()` with actionable messages, depends only on `core` | The **exact shape** for the video codec×transport matrix. SRT already collapses to `MpegTs`. **Gap:** it models track *count* carriage, **not** which *codec* a transport carries. |
| Capability detection keystone (unbuilt) | ADR-0035 / ADR-M007 `CapabilityReport`; `multiview-hal/src/capability.rs` `Capability { kind, stage, max_resolution, formats, decode_resize }` | The host-probe half. **`Capability` has no codec axis** (`rg -ic codec` → 0); `CapabilityReport` is documented but unbuilt (`rg CapabilityReport crates/` → 0). |

---

## 3. The five-piece design

### 3.1 Typed `Codec` catalog in `multiview-core` (new `multiview-core/src/codec.rs`)

Placement is **load-bearing**: `multiview-config/Cargo.toml` depends only on
`multiview-core` (plus serde/thiserror/toml) and **must not** depend on the FFI-owning
`multiview-ffmpeg`, where the enums live today. So the catalog **moves to `core`**;
`multiview-ffmpeg` re-exports it (`pub use multiview_core::codec::{VideoCodec, AudioCodec}`)
and keeps the libav encoder-name **resolvers** as impls/free-fns over the relocated enum —
no duplicate definition, existing `multiview_ffmpeg::codec::*` import paths unchanged. The
resolvers stay in `ffmpeg` (FFI + feature flags); `core` stays FFI-free and feature-free.

Shape (serde, `snake_case` **unit** variants → bare-string tokens, conventions §9-compliant —
a unit enum has no `untagged` risk):

```rust
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec { H264, H265, Av1, Mpeg2Video, Vp9, Vp8, Ffv1, Mjpeg }

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec { Aac, Opus, Ac3, Eac3, Mp2, PcmS16Le }
```

`VideoCodec::ALL` / `AudioCodec::ALL` const arrays are the enumerable catalog the
validator/API/UI iterate. Each variant exposes `entry() -> &'static CodecEntry`:
`{ family, wire_id: CodecWireId (RFC 6381 string + MPEG-TS stream_type), default_profiles,
license: LicenseClass }`.

> **AV1/VP9/VP8 are added to the ENCODE enum** (they exist on the decode enum but not the
> encode enum today) so the transport matrices are even expressible — Enhanced-RTMP-v2
> carries `av01`/`vp09`/`vp08` by FourCC; WebRTC mandates VP8. Because `VideoCodec` is
> `#[non_exhaustive]` with exhaustive `match` blocks in the resolvers, adding variants
> **forces** new resolver arms — see §6 for the honest, feature-gated fills.

### 3.2 `LicenseClass` — a property of **(codec, encoder backend)**, not the codec

```rust
#[non_exhaustive]
pub enum LicenseClass { LgplClean, GplEscalating, Proprietary }
```

The verified nuance (encoded today at `codec.rs`): **H.264/H.265 are `LgplClean` when
HW-encoded** (NVENC via MIT `nv-codec-headers` needs neither `--enable-gpl` nor
`--enable-nonfree`) but **`GplEscalating` when SW-encoded** (`libx264`/`libx265` taint the
whole build → GPL; gated behind `gpl-codecs`, off by default). Therefore license is keyed
on **(codec, backend)**, not flatly per-codec.

- `LgplClean`: native `aac` (never `libfdk_aac`), `libopus`, `mp2`, PCM, MPEG-2, FFV1,
  MJPEG; **AV1/VP9/VP8** (royalty-free, BSD/permissive encoders: `libaom`/`SVT-AV1`/`rav1e`,
  `libvpx`); **H.264/H.265 when HW-encoded** (NVENC).
- `GplEscalating`: H.264/H.265 **SW-encoded** (`libx264`/`libx265`).
- AC-3/E-AC-3: FFmpeg's **native** `ac3`/`eac3` encoders are **LGPL** — marked `LgplClean`
  **with a patent-pool obligation noted** (honest, not nonfree).
- `Proprietary`: NDI's wire codec — **runtime-loaded, separate `ndi` feature, not in this
  catalog** (NDI has no operator-chosen codec).

The catalog marks `license_escalating: bool` so the UI can flag `gpl-codecs` picks with a ⚠.

### 3.3 Transport×codec matrix — pure crate, mirroring the audio precedent

Mirror `multiview-audio/src/capability.rs` exactly: a transport enum + a pure-const
`for_transport()` returning the allowed-codec set + `validate_codec(transport, codec)
-> Result<(), CodecCarriageError>` with an actionable message. **No host involvement** — a
pure check. SRT folds into `MpegTs` (verified: the RTMP/SRT sink forces the `mpegts`
muxer for `Srt | UdpTs`, and the audio model already collapses SRT → `MpegTs`; make this a
single shared `transport_class()` mapping so audio and video cannot drift).

The matrix is **sourced from what the code can actually carry**, not from an abstract spec:
e.g. the RTSP allowed-set reads from `RtspCodec`'s payloader table (`caps.rs`), so adding
`rtpav1pay` to the server is the single edit that also widens the catalog's RTSP set — the
gate and the catalog can never disagree. See §4 for the full matrix.

RTMP is split into **two carriage classes**, mirroring the audio `SingleProgramOnly` /
Enhanced-negotiated split:

- **classic-FLV** — video `{H264}` only; audio `{Aac, PcmS16Le}` only.
- **Enhanced-RTMP-v2** (FourCC-signaled, endpoint-negotiated) — video `{H264, H265, Av1,
  Vp9, Vp8}`; audio adds `{Opus, Ac3, Eac3}`.

Default **conservative to classic** unless the endpoint negotiates Enhanced — so the safe
default rejects `h265`-on-RTMP (the operator's example) with a pure `CodecCarriageError`
naming *"requires an Enhanced-RTMP-v2 endpoint"*.

### 3.4 Codec×acceleration class — **derived** from the ADR-0035 host probe

The encode/decode resolvers exist; the **gap** is that `multiview-hal`'s `Capability` has
**no codec axis** (`rg -ic codec capability.rs` → 0), and ADR-0003 records *"NVML cannot
report codec capability."* ADR-0035's `CapabilityReport` (unbuilt) supplies the missing
per-(device, codec) `{ present, usable, in_use, reason }` via a **throwaway open** —
`ctx.open_as(h264_nvenc)` / `*_cuvid` at target resolution — because presence detection is
physically incapable of per-generation truth (a Pascal GTX 1660 and an Ada RTX 4090 present
an identical `/dev/nvidia0` to `EnvProbe`, which is presence-only). Per-generation facts
(**AV1 encode requires Ada+**, HEVC NVENC since Maxwell-2) **MUST** come from the
throwaway-open, refined over a static per-(vendor, gen, codec) prior — **never** from
`EnvProbe`'s presence-only `StageSupport`.

`AccelerationClass` is **derived, not stored**: collect every encode-supported **and
usable** backend for a codec on this host; `CPU = Software`, `GPU = Cuda | Vaapi | Qsv |
VideoToolbox | Metal | Wgpu`:

```rust
#[non_exhaustive]
pub enum AccelerationClass {
    CpuOnly,         // only Software encodes it
    GpuOnly,         // only GPU backend(s) encode it (no SW encoder at all)
    Both,            // Software AND >= 1 GPU backend
    DedicatedFuture, // reserved for a future ASIC backend
}
```

Present-but-unusable (NVENC linked but the session fails to open; AV1 NVENC on pre-Ada)
**downgrades** the badge to unsupported / CPU-only **with a `reason` HealthWarning** —
**never** a silent fallback. The class is computed from the **actual non-empty backend
set** the `CapabilityReport` reports usable, never from "no SW encoder exists, therefore
GPU" (see the corrected H.264 example in §5).

### 3.5 The validated surface — two phases + API + UI

- **Phase 1 (pure, `multiview-config`):** the codec field becomes `VideoCodec`/`AudioCodec`
  from the core catalog. `validate_outputs()` additionally checks
  `codec ∈ for_transport(transport)` — rejecting `h265`-on-classic-RTMP with **no hardware
  involved**.
- **Phase 2 (host, CLI/control layer holding the `CapabilityReport`):** a new
  `validate_with_capabilities(&self, &CapabilityReport)` (today `validate(&self)` takes only
  `&self`) rejects `av1`-NVENC-on-a-non-Ada-GPU with the report's `reason`, and **kills the
  silent fallback** in `pipeline.rs`: replace `_ => Mpeg2Video` and the warn+fallback with a
  hard `PipelineError::Codec { codec, reason }` (the variant already exists). Removing the
  fallback means a config naming a non-encodable codec now **fails the run** with a clear
  pre-flight rejection — so `validate_with_capabilities` **must run before run-start**, not
  as a late mid-pipeline crash.
- **API:** `GET /api/v1/system/capabilities/codecs?transport=<kind>` (ADR-M007 binding spec;
  management-capability-matrix.md *"`codec_support … GET …/devices/{id}/codecs … Gates
  Output dropdowns`"*) returns the **INTERSECTION** (catalog ∩ transport-allowlist ∩ host
  encode-support), each entry
  `{ id, label, kinds, acceleration, backends, license, license_escalating, supported,
  reason?, default }`. Read-only; reads a **cached `CapabilityReport` snapshot** — never
  back-pressures the engine (**invariant #10**).
- **UI:** replace the free-text `<TextField>` (default `'h264'`) with a `<CodecSelect>`
  dropdown (build on the existing `KindSelect` generic) fed by a
  `useCodecCapabilities(transport)` React-Query hook that re-queries when the Transport
  `KindSelect` changes. Each option **badged** with its acceleration class (CPU / GPU /
  CPU+GPU / dedicated) and a ⚠ license-escalating marker; unsupported codecs **greyed with
  the `reason` tooltip** (ADR-M007 *"impossible options greyed with reason"*); default comes
  from `default: true`, not the hard-coded `'h264'`. API client/types **generated** from the
  OpenAPI spec (`openapi-typescript`), not hand-written.

A codec change on a **live** output is a **Class-2** controlled reset (pinned session params,
make-before-break — management-capability-matrix.md). Key the Class-1-vs-Class-2 decision on
whether the output is **currently running**: a codec edit to a stopped/draft output is a free
(hot) create; the same edit to a live output triggers the Class-2 reset. The API/UI surface
this **before** applying.

---

## 4. The transport×codec matrix (the canonical table)

Transports are the delivery-layer carriage classes (one `Output` variant may map to one
transport class; SRT≡MPEG-TS). NDI is **omitted** — it has no operator-chosen codec.

| Transport (carriage) | Video codecs carried | Audio codecs carried | Track carriage | Source |
|---|---|---|---|---|
| **MPEG-TS** (program video) | H264 (`stream_type 0x1B`), HEVC (`0x24`), AV1 (`0x06` PES-private + AV01 registration descriptor), MPEG-2 (`0x02`) | AAC, MP2, AC-3, E-AC-3, Opus | `Multiple` (N PIDs) | ISO/IEC 13818-1 Table 2-34; AOM *Carriage of AV1 in MPEG-2 TS* |
| **SRT** | **= MPEG-TS** (SRT carries an MPEG-TS payload) | **= MPEG-TS** | `Multiple` | as-built: sink forces `mpegts` for `Srt`; audio model collapses SRT→`MpegTs` |
| **RTSP / RTP** | H264 (RFC 6184), HEVC (RFC 7798), AV1 (av1-rtp, rare/poor support — flag caution), MPEG-2 (RFC 2250) | AAC, MP2, AC-3, E-AC-3, Opus | `Multiple` (N `m=` subsessions) | matches as-built `RtspCodec { H264, H265 }` gate; **source the allowed-set from the payloader table** |
| **HLS / LL-HLS** (fMP4/CMAF) | H264 (**MANDATORY baseline — safe default**), HEVC (`hvc1`), AV1; +VP9 via CMAF/fMP4 | AAC (LC/HE/xHE), AC-3, E-AC-3, MP3 | `SelectOne` (one rendition) | RFC 8216 / draft-pantos-hls-rfc8216bis / Apple HLS authoring |
| **RTMP (classic FLV)** | **H264 only** | **AAC, PcmS16Le only** | `SingleProgramOnly` | legacy FLV/RTMP spec (catalog-intersected: no H.263/VP6 variants exist) |
| **RTMP (Enhanced-RTMP-v2)** | H264, HEVC, AV1, VP9, VP8 (FourCC) | AAC, Opus, AC-3, E-AC-3 (FourCC) + multitrack | endpoint-negotiated | Veovera Enhanced-RTMP-v2 (veovera.org) |
| **NDI** | *no operator codec* (SpeedHQ / NDI-HX H264·H265 / UYVY intrinsic) — **matrix skips NDI** | PCM-float (unlimited ch) / AAC (≤2 ch) / Opus (≤255 ch) | `ChannelMap` | docs.ndi.video |
| **WebRTC** *(PLANNED — no `Output` variant yet)* | VP8 + H264 Constrained-Baseline **MANDATORY** (RFC 7742); VP9/AV1/H265 optional | Opus + G.711 PCMU/PCMA **MANDATORY** (RFC 7874) | — | reserve as a future row; **do not expose** until a `WebRtc` variant exists |

**Corrections folded from review** (the classic-RTMP allowlist precision):

- Classic-RTMP video = `{H264}` **only** — explicitly excludes `Mpeg2Video`/`Ffv1`/`Mjpeg`/
  `H265`/`Av1`/`Vp9`/`Vp8`.
- Classic-RTMP audio = `{Aac, PcmS16Le}` — **NOT** "AAC/MP3": the catalog has no MP3
  variant; `Mp2` is not FLV-carryable; `Opus`/`Ac3`/`Eac3` are Enhanced-only.
- Enhanced video set is **catalog-intersected** (VVC omitted — no catalog variant; fine for
  a `#[non_exhaustive]` enum). Enhanced audio adds `{Opus, Ac3, Eac3}` (FLAC omitted — no
  catalog variant).

**The audio-codec×transport axis is the gap this catalog fills on top of the audio
precedent:** `capability.rs` models track *count* carriage (`Multiple`/`SelectOne`/
`SingleProgramOnly`/`ChannelMap`) but **not** which audio *codec* each transport allows. The
table's audio columns are the new axis.

---

## 5. The codec×acceleration matrix (per-host, derived)

| Codec | SW (LGPL default) | SW (GPL) | GPU (NVENC, `cfg cuda`) | Class derivation |
|---|---|---|---|---|
| **H264** (encode) | **NONE** (`lgpl_software_encoder()==None`) | `libx264` (`gpl-codecs`, **GplEscalating**) | `h264_nvenc` (NVENC, **LgplClean**) | see worked examples ↓ |
| **H265/HEVC** (encode) | **NONE** | `libx265` (`gpl-codecs`, **GplEscalating**) | `hevc_nvenc` (NVENC since Maxwell-2; Main10 since Pascal; **LgplClean**) | same as H264 |
| **AV1** (encode) | `libaom`/`SVT-AV1`/`rav1e` (permissive, **LgplClean**) — verify the linked FFmpeg actually has one | — | `av1_nvenc` **requires Ada (RTX 40xx)+** — reason `av1_encode_requires_ada+`; Intel Arc/Gen12.5+ (QSV/VAAPI); AMD VCN4/RDNA3; Apple VideoToolbox AV1 encode only M5 Pro/Max + M4 Ultra | per-gen fact **MUST** come from the ADR-0035 throwaway open, not `EnvProbe` |
| **VP9** (encode) | `libvpx-vp9` (**LgplClean**) | — | **NEVER NVENC** (VP9 GPU encode is VAAPI/QSV-only on supporting Intel gens) | do **not** add a `vp9_nvenc` arm |
| **VP8** (encode) | `libvpx` (**LgplClean**) | — | VAAPI/QSV on some Intel gens only, no NVENC | — |
| **MPEG2Video / FFV1 / MJPEG** (encode) | `mpeg2video`/`ffv1`/`mjpeg` (all LGPL, all builds) | — | none modelled | **`CpuOnly` on every host** (MPEG-2 is the silent-fallback target to eliminate) |
| **AAC / Opus / MP2** (encode) | `aac` (native) / `libopus` / `mp2` (all **LgplClean**, all builds) | — | none | **`CpuOnly`** always; `libfdk_aac` deliberately never modelled |
| **DECODE** (`cfg cuda`, `*_cuvid`) | — | — | H264→`h264_cuvid`, H265→`hevc_cuvid`, AV1→`av1_cuvid` (NVDEC AV1 Ampere+), VP9→`vp9_cuvid`, MPEG2→`mpeg2_cuvid`; SW fallback otherwise | decode set is **broader** than encode (adds AV1/VP9) — keep two axes per ADR-0026 |

**Worked examples (the corrected H.264 class derivation — this is the key review fix):**

- **MPEG-2** = `CpuOnly` always.
- **Pure default build** (`ffmpeg` only, no `cuda`, no `gpl-codecs`): H.264/H.265 =
  **Unsupported / NotEncodable** (empty candidate list) — badge **greyed** with reason
  *"no encoder in this build (needs `cuda` for NVENC or `gpl-codecs` for libx264/libx265)"*.
  **NOT `GpuOnly`** — the GPU encoder does not exist either without `cuda`. (The original
  design claim *"default build ⇒ H.264 is GPU-only"* was **refuted**: derive the class from
  the actual non-empty backend set, never assume a GPU backend exists merely because no SW
  one does.)
- **`cuda`-WITHOUT-`gpl-codecs`, host with a usable + correct-generation NVENC GPU:**
  `candidate_encoders(H264) = [h264_nvenc]` only → the **true `GpuOnly`** case (LgplClean).
- **`gpl-codecs`-only (no `cuda`):** H.264 = `CpuOnly` (`libx264`, GplEscalating).
- **`gpl-codecs` + `cuda`, usable NVENC:** H.264 = `Both`.
- **`cuda` host whose GPU lacks that encoder / `open_as` fails:** H.264 =
  **unsupported-on-GPU with reason** (downgrade, **never** a silent fallback).

**The seam:** the catalog supplies the **codec key space** (typed enum + license/wire
metadata); ADR-0035's runtime throwaway-open fills the **(codec, backend) → usable values**.
The `AccelerationClass` badge **MUST** consume the `CapabilityReport`'s per-(device, codec)
`{present, usable, reason}`, never `EnvProbe`'s presence-only `StageSupport` — otherwise the
UI would badge "av1 GPU" on a Pascal card.

---

## 6. Implementation hazards (folded from adversarial review — do not lose these)

1. **H.264/H.265 default-build class is Unsupported, not GpuOnly.** Compute from the actual
   backend set; the pure default build has an empty candidate list for H.264/H.265.
2. **AV1 NVENC needs Ada+; VP9 has NO NVENC.** When filling the forced new resolver arms:
   AV1 SW = `libaom-av1`/`librav1e` (verify presence in the linked FFmpeg); AV1 HW =
   `av1_nvenc` gated by `cuda` **and** runtime-gated on Ada+ via the throwaway open; VP9 HW =
   **none for NVENC** (do not add `vp9_nvenc`); VP8/VP9 GPU encode is VAAPI/QSV-only.
3. **Per-generation truth comes from the throwaway `open_as`, not presence.** `EnvProbe`
   reports `encode=Supported` for any present NVIDIA/VAAPI device without a per-codec query;
   ADR-0003 *"NVML cannot report codec capability."* Present-but-unusable → reason-bearing
   downgrade, never a badge.
4. **License is (codec, backend), not flat.** H.264/H.265 are LgplClean on NVENC,
   GplEscalating on `libx264`/`libx265`. Mark `license_escalating` per resolved backend.
5. **Native AAC only; no nonfree audio variant.** The guarantee is *"the catalog never
   SELECTS `libfdk_aac`"* (the name is never a candidate) — even if a system FFmpeg is built
   `--enable-nonfree`. AC-3/E-AC-3 = LgplClean-encoder-but-patent-pool-obligated, not nonfree.
6. **Killing the fallback changes failure semantics.** A config naming a non-encodable codec
   now fails the run; `validate_with_capabilities` must run **pre-flight** so the failure is
   a clear pre-start rejection with an actionable reason, not a late crash.
7. **Single source of truth (ADR-M007).** The same cached `CapabilityReport` gates the UI
   dropdown **and** is the validator's rejection source — never two sources (the ADR-M007
   alternative *"separate UI list from server validator"* is rejected: drift → UI offers what
   the server rejects).
8. **SRT≡MPEG-TS via one shared mapping.** Implement `transport_class()` once on the config
   `Output`, consumed by both the audio and the new video matrix, so they cannot drift; add a
   doc-note that the equivalence is conditional on the forced `mpegts` muxer.
9. **RTSP allowed-set sourced from the payloader table**, not a hardcoded `{H264,H265}`
   abstract truth, so adding `rtpav1pay`/`rtpvp9pay` widens the catalog in one edit and never
   advertises a codec the server cannot payload.
10. **Class-2 keys on running state.** A codec edit to a live output is Class-2
    (make-before-break); the same edit to a stopped/draft output is a free hot create.
