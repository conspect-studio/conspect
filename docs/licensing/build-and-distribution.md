# Build & distribution matrix + CI/release split (engineering companion)

> **Status:** engineering companion to [`relicense-advisory.md`](relicense-advisory.md). Grounded in
> the repo as of branch `feat/foundation-buildout` (2026-06-08). This document covers the part the
> licensing advisory's §6 implies but does not lay out end-to-end: **which binaries/containers may be
> public, which must be private, why, and the CI work to enforce it.** It is engineering advice, not
> legal advice.

## 1. Why there must be more than one build

The public *source* is offered under the source-available non-commercial license (see the repo
[`LICENSE`](../../LICENSE)). The public *binaries and containers* are a
**separate** question governed by **upstream** component licenses (LGPL/GPL/proprietary). The rule:

> **Any artifact whose contents are license-encumbered (GPL codecs, NDI) must NOT be
> published; it ships only through the gated commercial channel. Everything license-clean is public.**
> *(Native Dante is NOT supported per [ADR-T010](../decisions/ADR-T010.md); the open AES67 / ST 2110-30
> audio path is license-clean and public.)*

The good news (verified against the tree): the public half **already exists and is already
LGPL-clean**, and the only thing the public build loses is *software* H.264/H.265 — because Multiview
is GPU-first, hardware H.264/H.265 stays in the public build.

## 2. The encumbered-artifact list (exhaustive, as of today)

| Encumbrance | What it is in this repo | License | Public? |
|---|---|---|---|
| **`gpl-codecs`** | `libx264` (H.264) + `libx265` (H.265) **software** encoders — the *only* GPL trigger. Enabled by `--enable-gpl --enable-libx264 --enable-libx265` (`deploy/ffmpeg-build.sh:164–166`, `FF_LICENSE=gpl`) and selected at `crates/multiview-output/src/slate.rs:103`. | **GPL-2.0-or-later** | **NO** — `-gpl` images, private only |
| **NDI** | Vizrt NDI SDK, runtime-`dlopen`'d (`NDIlib_v6_load`), never vendored; gated by `NDI_RUNTIME_DIR_V6` + EULA acceptance. | Proprietary (Vizrt) | **NO** — private only |
*(Native Dante is **NOT supported** per [ADR-T010](../decisions/ADR-T010.md): it would require an
Audinate SDK that, as conveyed to us, is not available to open-source/community projects without a paid
commercial arrangement. It is not a planned feature and appears in no build. Dante facilities are reached
on the open AES67 / ST 2110-30 path below, which is license-clean and public.)*

**Everything else is license-clean and public**, including all *hardware* H.264/H.265:

| Capability | Why it's clean | Public? |
|---|---|---|
| **HW encode** — NVENC, VAAPI, QSV, VideoToolbox | Not GPL (`nv-codec-headers` is MIT; the rest are vendor/driver SDKs). `work-schedule.md:871`: *"NVENC/VAAPI/QSV/VideoToolbox are not GPL — LGPL-default stays clean."* | **YES** |
| **CPU fallback encode** — `mpeg2video` | LGPL. Why `pipeline.rs:326–345` falls back to MPEG-2 when `h264` is requested without `gpl-codecs`/GPU. | **YES** |
| **FFmpeg/libav** (demux/decode/filters) | LGPL-2.1, **dynamically linked** (`ffmpeg-sys-next`/`pkg-config`; `libav*.so.*` shipped in the image) — see §4. | **YES** |
| **Dante audio via AES67 / ST 2110-30** | Open standard, royalty-free, **zero Audinate IP** — Audinate's own license-free bridge. Multiview already owns the depacketizer (`crates/multiview-input/src/st2110/v30.rs`) + PTP servo (ADR-T010). | **YES** |
| **`libass`** subtitles | ISC-licensed — **not** a GPL trigger; off-by-default only for the C toolchain (`overlay-rendering.md:411`). | **YES** |

**The key insight:** `gpl-codecs` = exactly `libx264` + `libx265`, both *software* encoders. Every GPU
deployment gets H.264/H.265 from the LGPL-clean build, so the `-gpl` artifact is a narrow niche
(CPU-only H.264/H.265 on a GPU-less host). Keeping it private costs almost nothing.

## 3. The build / distribution matrix

| Build / image | Cargo features | FFmpeg | Contents | Combined-work license | Channel |
|---|---|---|---|---|---|
| **`multiview` (public)** | `ffmpeg,linux-vaapi,web` / `ffmpeg,nvidia,web` | **LGPL** (no `--enable-gpl`) | + HW encoders (NVENC/QSV/VAAPI/VT), wgpu/sw compositor, AES67/ST 2110-30 audio | Your **source-available non-commercial** license (✅ LGPL-compatible, §4) | **Public** (ghcr public, GitHub Releases) |
| **`multiview` + `-gpl`** | `…,gpl-codecs` | **GPL** (`FF_LICENSE=gpl`, +x264/x265) | + software H.264/H.265 | **GPL-2.0-or-later** — *cannot* carry the NC restriction (§4) | **Private** only / customer self-builds for private use |
| **`multiview` + NDI** | `…,ndi` | LGPL | + NDI send/receive (SDK runtime-loaded) | NC license + Vizrt NDI EULA (separate, upstream) | **Private**, commercial channel |

Notes that flow into the license model:

- **GPL can't be public** — distributing a `libx264` build forces GPL terms, which grant recipients
  full *commercial* freedom and erase the model. So it is never published. A paying H.264/H.265
  customer instead uses the **HW encoders** (no GPL at all), self-builds `gpl-codecs` for private
  internal use, or takes a **commercially-licensed** x264/x265 (x264 LLC / MulticoreWare) under their
  commercial agreement (+ MPEG-LA/Access Advance patent licensing, which is theirs to hold).
- **NDI is a *separate, upstream* proprietary license — not yours to grant.** A
  Multiview commercial licensee who wants native NDI still needs their **own** Vizrt
  relationship. Multiview never ships the SDK.
- **Native Dante is NOT supported** ([ADR-T010](../decisions/ADR-T010.md)): everyone reaching a Dante
  facility is served by the public **AES67/ST 2110-30** path, which needs nothing and ships in the
  public build.
- **"Dante" / "NDI" are third-party trademarks** — factual interop statements are nominative fair
  use; certification/branding claims are not.

## 4. LGPL container hygiene (public images) — obligation that exists regardless of relicensing

For the public images that bundle `libav*.so.*`, LGPL-2.1 §6 requires (advisory §6):

1. **Dynamic linking** of FFmpeg (already true) so the user can replace the `.so` — satisfied in a
   container by the libs being separate shared objects in a layer the user can rebuild/override.
2. The FFmpeg in the public image is compiled **without** `--enable-gpl`/`--enable-nonfree` and does
   **not** `apt install` `libx264`/`libx265` (the `deploy/ffmpeg-build.sh` guard hard-aborts on a GPL
   flag unless `FF_LICENSE=gpl`).
3. Carry FFmpeg's LGPL-2.1 text + attribution (`THIRD-PARTY-NOTICES`); state the work uses libav.
4. Neither the non-commercial **nor** the commercial EULA may forbid modifying/reverse-engineering the
   **FFmpeg portion** for debugging-and-relink (the most-missed LGPL trap — already carved out in
   the repo `LICENSE` §9).

## 5. Current CI state (verified) — public half is done; private lanes are the gap

| Workflow | What it does today | Public/private |
|---|---|---|
| `release.yml` | LGPL-clean release **binaries** (`--disable-gpl --disable-nonfree`), tar.gz + sha256 + provenance attestation. Comment line 10: *"the GPL `gpl-codecs` build is NOT a [release artifact]."* | **Public only** ✅ |
| `docker.yml` | App images to `ghcr.io/aperim/multiview` off `-lgpl`/`-nvidia` bases; features `ffmpeg,linux-vaapi,web` / `ffmpeg,nvidia,web`; provenance + SBOM + keyless cosign. | **Public only** ✅ |
| `ffmpeg-base.yml` | FFmpeg base images via `FF_HWACCEL`+`FF_LICENSE`. Also pushes a `multiview-ffmpeg:8.1.1-gpl` **base** (FFmpeg-only, GPL upstream) — fine in itself; **must never be combined into the published app image** under NC terms. | Mixed base; app stays clean |
| `ci.yml` | cargo-deny license gate where `gpl-codecs` *"fails the allowlist by design"* (line 170); `--all-features` deliberately avoided; NDI not a Cargo feature yet. | n/a |

**The GPL/NDI app path exists at the Dockerfile/`ffmpeg-build.sh` level but is not wired to any
*publishing* job.** That wiring — plus a hard "no-leak" guard — is the CI workstream. (Native Dante is
NOT a planned feature per [ADR-T010](../decisions/ADR-T010.md), so there is no Dante lane.)

## 6. CI/release split — the fan-out workstream (post-decision)

Structured as parallel teams (the operator asked for a fan-out):

- **Team A — public-lane "no-leak" gate.** Keep `release.yml`/`docker.yml`/`ffmpeg-base.yml`
  LGPL-clean and add a **content assertion** that *fails* the public job if an encumbered lib appears:
  `ffmpeg -encoders` shows no `libx264`/`libx265`; no `libndi`/Dante `.so` linked (`ldd`/manifest
  scan); cargo-deny on the exact published feature set. Belt-and-suspenders over the existing
  `ffmpeg-build.sh` abort.
- **Team B — private GPL lane.** Wire `FF_LICENSE=gpl` + `CARGO_FEATURES=…,gpl-codecs` into a publish
  job targeting a **private** ghcr package (e.g. `ghcr.io/aperim/multiview-gpl`) + `-gpl` binaries to a
  gated release — with provenance + cosign + SBOM parity.
- **Team C — private proprietary lane (NDI).** Same shape, never-vendored, fed by an
  operator-supplied SDK. **Blocked** until the feature lands — NDI-L1 awaits the operator's SDK URL.
  (Native Dante is NOT a planned feature per [ADR-T010](../decisions/ADR-T010.md) — no Dante lane;
  Team C is otherwise reserved for any future proprietary integration.)
- **Team D — access control + tag taxonomy + docs.** Private-package permissions scoped to commercial
  licensees; tag taxonomy (public vs `-gpl`/`-ndi`); provenance/SBOM/cosign across both
  channels; update `docs/operations/containerization.md` + `building.md` to mark which images are
  public vs commercial-only.

**Cost constraint:** private **ghcr** packages keep this free — no paid registry (matches the standing
no-paid-services rule). Provenance via `actions/attest-build-provenance`; signing via keyless cosign —
both already in use, both free.

**Gating / honest blockers** (why this is *planned*, not buildable now): (1) the license + commercial-
access model must be decided; (2) the private-channel mechanism chosen (private ghcr packages is the
default); (3) the NDI feature must exist (native Dante is NOT a planned feature — ADR-T010). All three are operator/decision-dependent → this
workstream **sequences after** the license is chosen, same gate as the doc refactor.
