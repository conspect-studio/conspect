# Research Brief — FFmpeg Build & Sourcing Strategy

> **Status:** verification-hardened (2026-06-07). Feeds [ADR-0031](../decisions/ADR-0031.md).
> **Scope:** how Multiview obtains libav\*, why we reject third-party FFmpeg builds for the
> product, the pinned target version + soname implications, the LGPL-clean / GPL configure
> split, static-vs-shared linkage, the reproducible multi-arch builder, and an honest
> roadmap for reducing FFmpeg reliance.
> **Source of truth for names/flags:** [`conventions.md` §7](../architecture/conventions.md).
> Where this brief and the Rust code disagree, the code wins — flag the drift.

---

## 0. TL;DR

- Multiview links libav\* through the **`ffmpeg-next`** Rust binding (+ its `ffmpeg-sys-next`
  build crate), opt-in behind the off-by-default `ffmpeg` feature. Pinned today at
  **`ffmpeg-next = "7.1"`** (`crates/multiview-ffmpeg/Cargo.toml:29`, mirrored in
  `crates/multiview-output/Cargo.toml:53` and `crates/multiview-cli/Cargo.toml:53`); the
  lockfile resolves **`ffmpeg-sys-next 7.1.3`** (`Cargo.lock:1298`), which tops out at
  **FFmpeg 7.1 / libavcodec 61**.
- The deploy images currently source libav\* from **third parties**: Debian-trixie apt
  (`deploy/Dockerfile`) and the **jellyfin-ffmpeg7 apt repo** with a `LD_LIBRARY_PATH`
  override (`deploy/Dockerfile.nvidia:92-109`). **We reject both for the product** —
  §1.
- **Decision (this brief recommends, ADR-0031 ratifies):** build our **own** FFmpeg from a
  pinned upstream tarball + SHA-256 + GPG verification, with our exact `--disable-everything`
  allowlist, an **LGPL-clean default** profile and a **separated GPL** profile, in a
  reproducible multi-arch builder stage that replaces jellyfin/PPA.
- **Target version:** ship **FFmpeg 7.1.4** first (latest 7.1 patch, libavcodec **61** —
  matches the pinned binding, *zero* code change), then a **gated** bump to **8.1.1**
  (libavcodec **62**) once the binding is moved to `ffmpeg-next = "8.1"` and verified
  `--locked`. The binding **already supports 8.1** (`ffmpeg-sys-next` 8.1.0's build.rs maps
  `ffmpeg_8_1` → libavcodec 62), so we are *not* capped at 7.x — it is a sequencing choice,
  not a ceiling.
- **NVENC/NVDEC, VAAPI, QSV (oneVPL), VideoToolbox all build LGPL-clean.** Only
  `cuda-nvcc`, `cuda-sdk`, `libnpp`, `libfdk_aac`, and `openssl` taint a build — and we use
  none of them. We use `--enable-cuda-llvm` + `nv-codec-headers` (no CUDA toolkit).
- **FFmpeg cannot be responsibly eliminated** — it is the only portable, multi-vendor,
  hardware-accelerated H.264/HEVC/AV1/VP9 codec engine. The honest lever is *shrinking* its
  surface to **codec + hwaccel only** (containers, protocols, packaging, depacketization,
  compositing already pure-Rust or in flight) — §6.

---

## 1. Why we reject a third-party FFmpeg for the product

The operator's objection is correct and the code confirms a concrete defect, not just unease:

1. **Opaque provenance.** A PPA/jellyfin blob is a foreign binary with no per-flag manifest we
   control. We cannot attest *what* is in it for an SBOM.
2. **Foreign patchset + release cadence.** jellyfin-ffmpeg carries a downstream patch series
   and ships on its own schedule, decoupled from upstream CVE timing and from our pin.
3. **Codec grab-bag → license ambiguity.** jellyfin-ffmpeg is built `--enable-gpl` with
   x264/x265 (and historically fdk-aac). It is therefore a **GPL (arguably nonfree) artifact**.
   Yet `deploy/Dockerfile.nvidia`'s header comment frames the NVIDIA runtime as the project's
   default — installing a GPL FFmpeg under a "default build" banner is a **real licensing
   mislabel** the own-build fixes. (The repo's own `gpl-codecs` cargo feature exists precisely
   to keep GPL opt-in; sourcing a GPL FFmpeg by default defeats it.)
4. **Soname coupling we don't own.** Today's images hand-match runtime `libav*.so.61` packages
   to the binding's expected major (`deploy/Dockerfile:118-119`); a third-party bump can break
   that silently.

Building our own removes all four: pinned source + checksum + GPG, our exact flags, our
soname line, our cadence, our SBOM.

---

## 2. The Rust binding — what it is, and the real version ceiling

- **Binding:** `ffmpeg-next` (safe-ish API) over **`ffmpeg-sys-next`** (the `-sys` build
  crate). Both **WTFPL** (permissive — no license escalation). Declared at
  `crates/multiview-ffmpeg/Cargo.toml:22-29`; opt-in behind `ffmpeg`
  (`crates/multiview-ffmpeg/Cargo.toml:33`).
- **Pinned/resolved today:** `ffmpeg-next 7.1.0` → `ffmpeg-sys-next 7.1.3`
  (`Cargo.lock:1287,1298`). That resolved build **only recognizes up to FFmpeg 7.1 /
  libavcodec 61.**
- **Real ceiling (verified live):** the latest `ffmpeg-sys-next` is **8.1.0**; its build.rs
  version array's three highest entries are `ffmpeg_8_1` → libavcodec **62**, `ffmpeg_8_0` →
  **62**, `ffmpeg_7_1` → **61**
  ([docs.rs/crate/ffmpeg-sys-next build.rs](https://docs.rs/crate/ffmpeg-sys-next/latest/source/build.rs)).
  `ffmpeg-next 8.1.0` (2026-03-18) depends on `ffmpeg-sys-next 8.1.0`
  ([crates.io](https://crates.io/crates/ffmpeg-next/versions),
  [docs.rs Cargo.toml.orig](https://docs.rs/crate/ffmpeg-next/latest/source/Cargo.toml.orig)).

  **Therefore we are NOT capped at 7.x.** Moving to FFmpeg 8.1 needs a one-line bump of the
  three `ffmpeg-next = "7.1"` declarations to `"8.1"`, `cargo update -p ffmpeg-sys-next`, and
  re-verifying `--locked`. The crate's README "3.4 … 8.0" line is stale relative to its own
  8.1 build.rs entry; rely on the build.rs array, not the prose.

  > **Stale-knowledge flag:** FFmpeg 8.x and `ffmpeg-next` 8.x are both post-training-cutoff
  > and were verified live (docs.rs build.rs, crates.io). Do **not** trust training memory that
  > says "ffmpeg-next caps at 7.x" or "7.1 is newest FFmpeg."

- **Discovery & linkage (verified from `ffmpeg-sys-next` build.rs):** three-tier — (1) a
  `build` cargo-feature that compiles FFmpeg from source (we do **not** use this; we want our
  own pinned tarball, not the crate's fetch); (2) **`FFMPEG_DIR`** env → `link-search` +
  include from `$FFMPEG_DIR/{lib,include}`; (3) **pkg-config** fallback. Static vs shared is
  the **`static` cargo-feature** (`CARGO_FEATURE_STATIC` → `rustc-link-lib=static=avcodec`
  vs `dylib=avcodec`). Cross-compile honours `SYSROOT`/`CC_*`/`CFLAGS_*`. We point cargo at
  our own-built prefix via `PKG_CONFIG_PATH=<prefix>/lib/pkgconfig` (our `--prefix` install
  ships the `.pc` files) **or** `FFMPEG_DIR=<prefix>`.
  ([rust-ffmpeg-sys build.rs](https://github.com/zmwangx/rust-ffmpeg-sys/blob/master/build.rs))

---

## 3. Target version + soname implications

| Branch | Latest patch | Date | libavcodec / libavutil / libavformat | libavfilter / swscale / swresample |
|---|---|---|---|---|
| **7.1 "Péter"** (FF-0 target) | **7.1.4** | 2026-05-05 | **61 / 59 / 61** | 10 / 8 / 5 |
| **8.1 "Hoare"** (FF-0b target) | **8.1.1** | 2026-05-04 | **62 / 60 / 62** | 11 / 9 / 6 |

Sources: [ffmpeg.org/download.html](https://ffmpeg.org/download.html),
[endoflife.date/ffmpeg](https://endoflife.date/ffmpeg),
[archlinux ffmpeg 8.1.1](https://archlinux.org/packages/extra/x86_64/ffmpeg/),
[LWN: FFmpeg 8.0 libavcodec 62 bump](https://lwn.net/Articles/1034813/).

- **8.0 and 8.1 share sonames (62/60/62)** → 8.0↔8.1 is ABI-drop-in. **7.1→8.x is NOT** —
  every `.so.NN` major changes (61→62, 59→60, 8→9, …). The repo references the 7.1 soname
  line in both Dockerfiles (`deploy/Dockerfile:118-119`) and a `cuvid_name` doc comment
  (`crates/multiview-ffmpeg/src/hwdecode.rs:118` says "FFmpeg 7.1").
- **Why 7.1.4 first:** it is the latest 7.1 patch (security fixes, **same soname 61** the
  pinned binding expects). FF-0 can replace jellyfin/PPA with **zero Rust-code change** and
  zero soname churn — the cheapest possible way to land the provenance win.
- **Why 8.1.1 is a gated follow-on, not the FF-0 target:** the bump touches the binding pin,
  the lockfile, every soname reference in both Dockerfiles, and the `hwdecode.rs:118` comment,
  and must be re-verified `--locked`. Sequencing it *after* the builder exists keeps each PR
  small and reversible. The builder takes `FFMPEG_VERSION` as an ARG so the bump is a one-line
  change.

---

## 4. The configure split

The licensing gate, settled from the authoritative source — FFmpeg's own `configure` script:

```
HWACCEL_LIBRARY_NONFREE_LIST="  cuda_nvcc   cuda_sdk   libnpp  "
EXTERNAL_LIBRARY_GPL_LIST="  ... libx264  libx265 ... "
```
([FFmpeg 7.1 configure](https://raw.githubusercontent.com/FFmpeg/FFmpeg/release/7.1/configure),
[LICENSE.md](https://github.com/FFmpeg/FFmpeg/blob/master/LICENSE.md),
[legal.html](https://www.ffmpeg.org/legal.html))

**Load-bearing fact:** `nvenc`, `nvdec`, `cuvid`, `ffnvcodec`, `cuda-llvm`, `vaapi`,
`libvpl`(QSV), `videotoolbox` are **NOT** on the nonfree or GPL lists. Only `cuda_nvcc`,
`cuda_sdk`, `libnpp` (nonfree), `libx264`/`libx265` (GPL), `libfdk_aac`/`openssl` (nonfree)
taint. **We use none of the taints.** We scale with `scale_cuda` (LGPL, JIT-compiled by
`--enable-cuda-llvm`), never `scale_npp` — exactly the CLAUDE.md §7 rule
(`crates/multiview-ffmpeg/src/hwdecode.rs:222-225`).

> **Folklore flag:** NVIDIA's own build guide recommends
> `--enable-nonfree --enable-cuda-nvcc --enable-libnpp` and claims the CUDA toolkit is
> required. That is the **NPP/nvcc** path. We do **not** need it — `--enable-cuda-llvm` +
> `nv-codec-headers` gives NVENC/NVDEC/`scale_cuda` **LGPL-clean, without the CUDA SDK**. Do
> not copy NVIDIA's nonfree line.
> ([NVIDIA FFmpeg-with-GPU guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html))

### 4a. The exact libav surface Multiview uses (so we can `--disable-everything`)

Read directly from the code:
- **Encoders — LGPL default:** `mpeg2video, ffv1, mjpeg, rawvideo` + audio `aac` (native,
  **never** `libfdk_aac` — explicitly asserted at `crates/multiview-ffmpeg/src/codec.rs:351-361`),
  `libopus, mp2, flac, pcm_s16le` (`codec.rs:64-70,197-199`, `multiview-output/src/sink.rs:199,236-237`).
- **Encoders — GPL (`gpl-codecs`):** `libx264, libx265` (`codec.rs:80-81`).
- **Encoders — NVENC (`cuda`):** `h264_nvenc, hevc_nvenc` (`codec.rs:100-101`).
- **Decoders:** software `h264/hevc/av1/vp9/mpeg2` + NVDEC `*_cuvid`
  (`h264_cuvid, hevc_cuvid, av1_cuvid, vp9_cuvid, mpeg2_cuvid`,
  `crates/multiview-ffmpeg/src/hwdecode.rs:121-129`); hwaccel device types `cuda, vaapi, qsv,
  videotoolbox` (`hwdecode.rs:66-72`).
- **Muxers:** `mpegts` (HLS segments + SRT/UDP-TS push), `mp4`/`mov` (fMP4 init),
  `flv` (RTMP push), `rtsp`/`rtp`, `adts` (`multiview-output/src/sink.rs`;
  `PushProtocol::muxer_name` maps `Rtmp→"flv"`, `Srt|UdpTs→"mpegts"`, `Rtsp→"rtsp"` at
  `sink.rs:1034-1040,1109,1163`).
- **Protocols:** `file, pipe, tcp, udp, rtp, rtsp, rtmp, hls, https, tls, crypto, data, srt`.
- **swscale/swresample:** only for decoder-output→NV12 fixup and audio resample at the encode
  boundary — the **wgpu compositor owns scale/place/color**, so swscale is otherwise redundant.

**Out of libav's scope (already ours, do NOT enable libav equivalents):** HLS/LL-HLS
packaging + playlists (`multiview-output/src/hls/`), the in-process **RTSP server** (built on
**GStreamer** via the `rtsp-server` feature — `multiview-output/Cargo.toml:34-36`, *not*
libav), MPEG-TS PSI/SI parse on ingest (`multiview-input/src/mpegts/`), WebRTC (pure-Rust
shell). So the custom build needs libav's RTSP/RTP only for **client-side push**, not serving.

### 4b. LGPL-clean default `./configure`

```bash
./configure \
  --prefix=/opt/multiview-ffmpeg \
  --disable-everything \
  --enable-shared --disable-static \           # default linkage = shared (see §5)
  --disable-programs --disable-doc \           # libs only (BUT keep ffprobe — see note)
  --disable-debug --disable-autodetect \       # reproducible: never absorb host libs implicitly
  \
  # decoders (software fallback + what we ingest)
  --enable-decoder=h264,hevc,av1,vp9,mpeg2video,mjpeg,ffv1,aac,opus,mp2,ac3,pcm_s16le \
  # encoders (LGPL-clean only)
  --enable-encoder=mpeg2video,ffv1,mjpeg,rawvideo,aac,libopus,mp2,flac,pcm_s16le \
  # parsers + bitstream filters (mux H.264/HEVC into TS/MP4/FLV)
  --enable-parser=h264,hevc,av1,vp9,aac,opus,mpegaudio,mpegvideo \
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata,aac_adtstoasc,null \
  # demuxers / muxers (ingest + the exact output containers)
  --enable-demuxer=mpegts,hls,mov,flv,rtsp,rtp,sdp,aac,matroska,h264,hevc,data \
  --enable-muxer=mpegts,mp4,mov,flv,rtsp,rtp,adts,data \
  # protocols
  --enable-protocol=file,pipe,tcp,udp,rtp,rtsp,rtmp,hls,https,tls,crypto,data,srt \
  # filters: we composite ourselves; keep only conversion + hw scale/format
  --enable-filter=scale,format,aformat,aresample,anull,null,fps,setpts,asetpts \
  --enable-swscale --enable-swresample --enable-avfilter \
  # external LGPL libs
  --enable-libopus \                           # LGPL
  --enable-libsrt \                            # SRT (MPL-2.0, LGPL-compatible)
  --enable-gnutls \                            # TLS for https/rtmps (LGPL — NOT openssl)
  # hardware: ALL LGPL-clean
  --enable-ffnvcodec --enable-nvenc --enable-nvdec --enable-cuvid \  # NVIDIA via nv-codec-headers
  --enable-cuda-llvm \                          # clang JITs scale_cuda — LGPL (NOT cuda-nvcc/libnpp)
  --enable-vaapi --enable-libdrm \             # Intel/AMD Linux
  --enable-libvpl \                            # Intel QSV via oneVPL (amd64 only — guard per-arch)
  --enable-videotoolbox                        # macOS
  # ABSENT BY DESIGN: --enable-gpl, --enable-nonfree, --enable-libnpp,
  #                   --enable-cuda-nvcc, --enable-libx264/5, libfdk_aac, --enable-openssl
```

Notes:
- **`ffprobe`:** invariant #8 ("verify output with ffprobe") and the output tests shell out to
  `ffprobe`. **Keep it** — drop `--disable-programs` and instead pass
  `--disable-ffplay` (and `--disable-ffmpeg` if the CLI binary is unwanted), so a small
  `ffprobe` is installed. **Confirm at FF-0** that the runtime/test image actually needs the
  `ffprobe` binary present vs. linking probe via the library; default to *shipping ffprobe*.
- **`--enable-version3`:** **omit by default** → keep the build **LGPL v2.1+** (simplest "LGPL
  clean"). Add it only if a build-time `config.log` audit shows a *used* component needs v3.
  (Confidence: HIGH that omitting it is fine for the listed components; verify in `config.log`.)
- **`--enable-libvpl`** is the current Intel QSV path (oneVPL); the old `--enable-libmfx` is
  deprecated. `libvpl`/QSV is **x86_64-only** — guard behind `[ "$TARGETARCH" = "amd64" ]`.
- **nv-codec-headers**, not the CUDA SDK, is the NVENC build dependency. Pin a **tag** (current:
  `n13.0.19.0`, Video Codec SDK 13.0.19, Linux driver ≥570) — a mismatched header fails
  configure with "nvenc API version not match." ([nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers))
- **Validate the allowlist by building once** and checking `ffmpeg -muxers/-decoders` /
  `config.log`. The classic silent breakage is a missing parser/bsf for H.264-into-MP4/TS.

### 4c. GPL variant (separate artifact + image tag only)

```bash
#   ... identical to 4b, PLUS:
  --enable-gpl \
  --enable-libx264 --enable-encoder=libx264 \
  --enable-libx265 --enable-encoder=libx265
#   → whole build becomes GPLv2+. NEVER the default. Tagged `-gpl`.
#     Flows from a FF_LICENSE=gpl builder ARG; pairs with the existing `gpl-codecs`
#     cargo feature (crates/multiview-cli/Cargo.toml) and the `-gpl` image arg in deploy/.
```

---

## 5. Static vs shared linkage

The runtime must provide the **same libavcodec soname major** linked at build (61 for 7.1, 62
for 8.x) — the exact coupling today's Dockerfiles hand-manage. Two defensible postures:

| | **Shared, our-built (FF-0 default)** | **Static (option, FF-3)** |
|---|---|---|
| Provenance | Our tarball/flags, our `.so.61` bundled in runtime | Our FFmpeg baked into one `multiview` binary |
| Runtime soname coupling | Present, but **we own both ends** (no third party) | **None** — no `LD_LIBRARY_PATH`, no `.so` matching |
| Drops jellyfin/PPA | **Yes** | **Yes** |
| LGPL obligation | Trivially satisfied (dynamic) | LGPL-2.1 §6 relinkability: must ship the FFmpeg source/object — **satisfied** because we already ship the pinned source tarball + checksum |
| OS-level CVE patch | Patch `.so` independently | Rebuild binary |
| Multi-arch | Per-arch `.so` set in each runtime | Per-arch static binary |

**Recommendation:** **FF-0 ships shared, our-built** libav\* (`--enable-shared
--disable-static`, copied from the builder stage into the runtime) — it kills jellyfin/PPA
*and* keeps the LGPL story trivial with no relinkability paperwork. **Offer static as FF-3**
(the single-binary, zero-soname-coupling property) once the licensing lane confirms the §6
relinkability handling (we already ship the source tarball, so the obligation is met). The
binding selects this via the `ffmpeg-sys-next` `static` cargo-feature pointed at our prefix.

> Lane note: the rust-binding lane preferred static-first; the builder lane preferred
> shared-first. Resolved **shared-first** because it lands the provenance win with the least
> licensing surface and matches the repo's existing shared-`.so` runtime shape; static is a
> clean, well-scoped follow-on (FF-3), not a blocker.

---

## 6. The reproducible multi-arch builder

A shared builder stage (`deploy/Dockerfile.ffmpeg`, or a `FROM … AS ffmpeg-build` stage)
emitting a staging prefix `/opt/multiview-ffmpeg`:

```dockerfile
FROM debian:trixie@sha256:<pinned> AS ffmpeg-build
ARG FFMPEG_VERSION=7.1.4
ARG FFMPEG_SHA256=<pin-TODO>          # ffmpeg.org/releases/ffmpeg-7.1.4.tar.xz
ARG NVCODEC_TAG=n13.0.19.0
ARG NVCODEC_SHA256=<pin-TODO>
ARG FF_LICENSE=lgpl                   # lgpl | gpl
# 1. provenance: curl tarball + .asc; gpg --recv-key FCF986EA15E6E293A5644F10B4322F04D67658D8;
#                gpg --verify; sha256sum -c
# 2. nv-codec-headers @ $NVCODEC_TAG (pinned tarball+sha) → make install PREFIX=/opt/multiview-ffmpeg
# 3. ./configure  (§4b for lgpl; + §4c for gpl)  — flags conditioned on $TARGETARCH/$FF_LICENSE
# 4. make -j"$(nproc)" && make install  →  /opt/multiview-ffmpeg/{lib,include,bin}
```

- **Multi-arch (linux/amd64 + linux/arm64):** prefer **per-arch native build under
  buildx/QEMU** (`--platform=$TARGETPLATFORM`) over cross-toolchains — it gets the right
  per-arch vaapi/qsv libs. **NVENC is arch-portable** (headers only; the driver `.so` is
  injected at runtime). **QSV (`libvpl`) is amd64-only** — guard the flag behind
  `[ "$TARGETARCH" = "amd64" ]` (mirrors the existing amd64 arch-guard at `deploy/Dockerfile:121`).
  `libva`/`libdrm` exist on both arches.
- **Cache the builder as its own image** (e.g. `ghcr.io/<org>/multiview-ffmpeg:7.1.4-lgpl-amd64`),
  referenced by digest, so the ~5–15 min compile runs only when `FFMPEG_VERSION`/flags/
  `NVCODEC_TAG` change — not every app rebuild. `buildx --cache-to/--cache-from` keyed on
  those ARGs.
- **Reproducibility:** pinned tarball + SHA-256 + **GPG-verified** source
  (key `FCF986EA15E6E293A5644F10B4322F04D67658D8`) + pinned `nv-codec-headers` tag + frozen
  configure flags + pinned `debian:trixie@sha256:` toolchain + `SOURCE_DATE_EPOCH` →
  deterministic libav\*. Record resolved digests in CI provenance/SBOM.

### 6a. How it slots into the existing Dockerfiles

- **`deploy/Dockerfile` (LGPL/software+vaapi):** in the *builder*, replace
  `apt-get install … libav*-dev` (≈`:71-77`) with
  `COPY --from=ffmpeg-build /opt/multiview-ffmpeg /opt/multiview-ffmpeg` +
  `ENV PKG_CONFIG_PATH=/opt/multiview-ffmpeg/lib/pkgconfig LD_LIBRARY_PATH=/opt/multiview-ffmpeg/lib`
  — `ffmpeg-sys-next` then finds our build via pkg-config automatically. In the *runtime*
  (`:115-124`), drop the `libavcodec61 libavformat61 …` apt packages; instead
  `COPY --from=ffmpeg-build /opt/multiview-ffmpeg/lib /usr/local/lib && ldconfig`. **Keep**
  `libva2`/`mesa-va-drivers` (drivers, not FFmpeg).
- **`deploy/Dockerfile.nvidia`:** **delete the entire jellyfin block (`:92-109`)** — the
  `jellyfin.gpg`/apt-repo install and `LD_LIBRARY_PATH=/usr/lib/jellyfin-ffmpeg/lib`. Replace
  with the same `COPY … /usr/local/lib && ldconfig`. Our LGPL build *includes*
  `h264_nvenc`/`hevc_nvenc` (we pass `--enable-nvenc`), so the `select_encoder` fallback-to-
  mpeg2 concern the current comment describes **disappears without going GPL**. Keep
  `NVIDIA_DRIVER_CAPABILITIES=compute,video,utility`.
- **`-gpl` tag:** same Dockerfiles, `--build-arg FF_LICENSE=gpl` flows to the ffmpeg-build
  stage; cargo built with the existing `gpl-codecs` feature.

### 6b. Own-build vs jellyfin-ffmpeg — honest scorecard

| Axis | Own-build | jellyfin-ffmpeg / PPA |
|---|---|---|
| Provenance | pinned tarball + GPG + SHA-256 | opaque blob, foreign patchset |
| License control | explicit LGPL/GPL split, never nonfree | ships GPL x264/x265 under a "default" banner |
| Exact codecs | we choose the flags | grab-bag |
| Soname ↔ binding | we build the 61 (then 62) line | happens to be 61 today |
| Reproducible | byte-stable | no |
| NVENC in LGPL build | yes (`--enable-nvenc`, no GPL) | only via their GPL build |
| Build time | +5–15 min/arch (cacheable) | apt seconds |
| Upstream CVE upkeep | **we own it** | upstream maintains |

The two real costs of own-build — build time and CVE-tracking ownership — are bounded
(cacheable builder image; we already track deps via `cargo deny`). They are the price of the
provenance/licensing control that is the entire point.

---

## 7. Honest roadmap — reducing FFmpeg reliance

**FFmpeg cannot be responsibly eliminated.** There is **no** production-grade pure-Rust
H.264/HEVC *encoder* (`less-avc` is lossless all-intra only; `rav1e` is AV1, not the H.264/HEVC
we emit), and pure-Rust *decoders* are research-grade and not perf-competitive for live
(`rust_h265` author-claims byte-exactness on fixtures but is unproven at scale; `rust_h264`
similar). Hardware encode/decode (NVENC/NVDEC/VAAPI/QSV/VideoToolbox) *are* the vendor SDKs —
FFmpeg is just the portable, multi-vendor wrapper that `multiview-ffmpeg/hwframe.rs` /
`hwdecode.rs` get "for free." The defensible posture is **shrink the surface to codec +
hwaccel only**, not removal.

**Already pure-Rust / FFmpeg-free in this repo (the unease is partly already addressed):**
HLS/LL-HLS packaging + playlists (`multiview-output/src/hls/`), DASH/MPD (`dash/`),
MPEG-TS PSI/SI ingest parse (`multiview-input/src/mpegts/`), ST 2110 + ST 2022-6/-7 RTP
depacketizers (`multiview-input/src/st2110/`, `st2022_*.rs`), SCTE-35/104 (`scte/`), the RTSP
*server* (GStreamer), WebRTC SDP (pure-Rust shell), NDI (own `multiview-ndi-sys`), and the
wgpu compositor (owns scale/place/color → swscale already redundant on the program path).

**Credible phased reductions (research-gated, not commitments):**
1. **Egress containers → pure Rust (low risk, high payoff).** Replace the libav `Muxer` for
   `mpegts` (UDP/SRT payload), `flv` (RTMP payload), and CMAF/fMP4 — Multiview already owns TS
   parsing + HLS/CMAF segmentation, so this directly attacks `Muxer::create_as`
   (`multiview-output/src/sink.rs:1109,1163`). Candidate crates: a small owned TS muxer;
   `muxide`/`mp4` family for fMP4; `rml_rtmp` carries FLV. **Codecs stay FFmpeg; only the
   container leaves.**
2. **Ingest protocols → pure Rust (medium risk).** `retina` (RTSP — production in Moonfire NVR,
   but self-described lightly-tested depacketizers), `rml_rtmp` (RTMP 1.0), `srt-tokio`
   (`srt-protocol` state machine). Behind feature flags, A/B-tested against the libav path;
   coded frames feed FFmpeg decode. Removes the `libsrt`/network-demux surface; Multiview
   already owns the SRT *config* model + RTP depacketization.
3. **Direct hardware codec bindings → drop libavcodec on HW paths (large, optional).**
   `nvidia-video-codec-sdk`/`nvcodec-rs` (NVENC), Rust VideoToolbox + VA-API crates — only if
   SDK provenance becomes a hard requirement. Re-creates a major slice of `hwframe.rs`;
   software encode (x264/x265 via FFmpeg) stays the fallback indefinitely.

**Steady state:** FFmpeg as a **codec + hwaccel-only** dependency — a smaller, easier-to-attest
build — with containers, protocols, packaging, depacketization, and compositing owned in pure
Rust (much already in-house). Own-build + a documented "FFmpeg is codec/hwaccel only" boundary
is the right posture; elimination is not.

> **Confidence/staleness:** FFmpeg 8.x facts verified live (post-cutoff). Protocol/codec
> crate maturity ratings are directional (web summaries, not a code audit) — Phase-2 crate
> selection needs a hands-on spike. Pure-Rust decoder byte-exactness claims are author-stated,
> unverified here.

---

## 8. Sources

Code (file:line): `crates/multiview-ffmpeg/Cargo.toml:22-33`,
`crates/multiview-ffmpeg/src/codec.rs:64-70,80-81,100-101,197-199,351-361`,
`crates/multiview-ffmpeg/src/hwdecode.rs:66-72,118,121-129,222-225`,
`crates/multiview-output/src/sink.rs:199,236-237,1034-1040,1109,1163`,
`crates/multiview-output/Cargo.toml:34-36,53`, `crates/multiview-cli/Cargo.toml:53`,
`Cargo.lock:1287,1298`, `deploy/Dockerfile:71-77,115-124`, `deploy/Dockerfile.nvidia:92-109`.

Web: [ffmpeg.org/download.html](https://ffmpeg.org/download.html) ·
[FFmpeg 7.1 configure (license lists)](https://raw.githubusercontent.com/FFmpeg/FFmpeg/release/7.1/configure) ·
[FFmpeg LICENSE.md](https://github.com/FFmpeg/FFmpeg/blob/master/LICENSE.md) ·
[FFmpeg legal.html](https://www.ffmpeg.org/legal.html) ·
[ffmpeg-next crates.io](https://crates.io/crates/ffmpeg-next/versions) ·
[ffmpeg-sys-next build.rs (highest entry ffmpeg_8_1 → libavcodec 62)](https://docs.rs/crate/ffmpeg-sys-next/latest/source/build.rs) ·
[rust-ffmpeg-sys build.rs (FFMPEG_DIR/pkg-config/static)](https://github.com/zmwangx/rust-ffmpeg-sys/blob/master/build.rs) ·
[NVIDIA FFmpeg-with-GPU guide](https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/ffmpeg-with-nvidia-gpu/index.html) ·
[nv-codec-headers](https://github.com/FFmpeg/nv-codec-headers) ·
[endoflife.date/ffmpeg](https://endoflife.date/ffmpeg) ·
[archlinux ffmpeg 8.1.1](https://archlinux.org/packages/extra/x86_64/ffmpeg/) ·
[LWN FFmpeg 8.0 soname bump](https://lwn.net/Articles/1034813/) ·
[retina](https://crates.io/crates/retina) · [rml_rtmp](https://docs.rs/rml_rtmp) ·
[srt-rs](https://github.com/russelltg/srt-rs) · [less-avc](https://github.com/strawlab/less-avc) ·
[rust_h265](https://docs.rs/rust_h265) · [muxide](https://github.com/Michael-A-Kuykendall/muxide) ·
[nvidia-video-codec-sdk](https://crates.io/crates/nvidia-video-codec-sdk).
