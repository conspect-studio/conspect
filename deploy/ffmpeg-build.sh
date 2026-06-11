#!/usr/bin/env bash
#
# deploy/ffmpeg-build.sh — build Multiview's OWN pinned FFmpeg from source.
#
# Ratifies ADR-0031 (FF-0b): replace the third-party libav* (jellyfin-ffmpeg7 on
# the NVIDIA image, Debian-trixie apt libav*-dev on the VAAPI image) with a
# pinned, GPG- AND SHA-256-verified FFmpeg 8.1.1 source build under OUR exact
# `--disable-everything` allowlist. This is the single, audited source of the
# configure flag list — both deploy/Dockerfile and deploy/Dockerfile.nvidia call
# this one script, so the LGPL-clean guardrail (no GPL/nonfree leak) lives in
# exactly one place.
#
# Soname target: FFmpeg 8.1.1 → libavcodec.so.62 / libavutil.so.60 /
# libavformat.so.62 / libavfilter.so.11 / libswscale.so.9 / libswresample.so.6.
# This matches `ffmpeg-sys-next 8.1.0` (build.rs maps `ffmpeg_8_1` → libavcodec
# 62), the version the workspace lockfile resolves, so `cargo build` links .so.62.
#
# Inputs (environment variables, supplied by the Dockerfile via ARG→ENV):
#   FFMPEG_VERSION   FFmpeg release to build               (default 8.1.1)
#   FFMPEG_SHA256    SHA-256 of ffmpeg-${FFMPEG_VERSION}.tar.xz  (REQUIRED)
#   FFMPEG_GPG_KEY   FFmpeg release signing key fingerprint (default below)
#   FF_LICENSE       lgpl (default) | gpl                  (gpl adds x264/x265)
#   FF_HWACCEL       nvidia (NVENC/NVDEC/cuvid/scale_cuda) | vaapi (VAAPI/QSV)
#   FF_PREFIX        install prefix                        (default /opt/multiview-ffmpeg)
#   NVCODEC_COMMIT   nv-codec-headers git commit (nvidia only) — content-pinned
#   TARGETARCH       Docker buildx arch: amd64 | arm64     (gates QSV/libvpl)
#
# Provenance (ADR-0031 §"Source provenance"): the tarball is verified by BOTH a
# detached GPG signature (.asc, key FCF986EA15E6E293A5644F10B4322F04D67658D8) AND
# a pinned SHA-256. Either failing aborts the build. Verified end-to-end at
# author time against the real ffmpeg.org tarball:
#   GPG  : Good signature from "FFmpeg release signing key <ffmpeg-devel@ffmpeg.org>"
#          primary key fingerprint FCF986EA 15E6 E293 A564 4F10 B432 2F04 D676 58D8
#   SHA256 b6863adde98898f42602017462871b5f6333e65aec803fdd7a6308639c52edf3
#          (independently confirmed: Fossies SHA-256, rpmfusion SHA-512).
#
# LGPL-clean guardrail (ADR-0031 §"Enable-flags", CLAUDE.md §7): the default
# profile passes NO --enable-gpl, NO --enable-nonfree, NO --enable-libnpp,
# --enable-cuda-nvcc, --enable-libx264/265, --enable-libfdk_aac, --enable-openssl.
# NVENC/NVDEC/cuvid/cuda-llvm/vaapi/libvpl/gnutls/libsrt/libopus are all
# LGPL-clean. We scale with scale_cuda (--enable-cuda-llvm), never scale_npp.
set -euo pipefail

FFMPEG_VERSION="${FFMPEG_VERSION:-8.1.1}"
FFMPEG_GPG_KEY="${FFMPEG_GPG_KEY:-FCF986EA15E6E293A5644F10B4322F04D67658D8}"
FF_LICENSE="${FF_LICENSE:-lgpl}"
FF_HWACCEL="${FF_HWACCEL:-vaapi}"
FF_PREFIX="${FF_PREFIX:-/opt/multiview-ffmpeg}"
NVCODEC_COMMIT="${NVCODEC_COMMIT:-e844e5b26f46bb77479f063029595293aa8f812d}"
TARGETARCH="${TARGETARCH:-amd64}"

if [ -z "${FFMPEG_SHA256:-}" ] || [ "${FFMPEG_SHA256}" = "TODO-pin" ]; then
  echo "ffmpeg-build: FFMPEG_SHA256 is not pinned — refusing to build unverified source." >&2
  echo "ffmpeg-build: set --build-arg FFMPEG_SHA256=<sha256 of ffmpeg-${FFMPEG_VERSION}.tar.xz>." >&2
  exit 2
fi

echo "ffmpeg-build: FFmpeg ${FFMPEG_VERSION} | license=${FF_LICENSE} | hwaccel=${FF_HWACCEL} | arch=${TARGETARCH} | prefix=${FF_PREFIX}"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
cd "${WORK}"

# -----------------------------------------------------------------------------
# 1. Fetch + verify the pinned source tarball (GPG signature AND SHA-256).
# -----------------------------------------------------------------------------
TARBALL="ffmpeg-${FFMPEG_VERSION}.tar.xz"
BASE_URL="https://ffmpeg.org/releases"

curl -fsSL -o "${TARBALL}"       "${BASE_URL}/${TARBALL}"
curl -fsSL -o "${TARBALL}.asc"   "${BASE_URL}/${TARBALL}.asc"

# SHA-256 (defense-in-depth pin).
echo "${FFMPEG_SHA256}  ${TARBALL}" | sha256sum -c -

# GPG detached-signature verification against the pinned FFmpeg release key.
export GNUPGHOME="${WORK}/gnupg"
mkdir -p "${GNUPGHOME}"
chmod 700 "${GNUPGHOME}"
gpg_recv_ok=0
for ks in hkps://keyserver.ubuntu.com hkps://keys.openpgp.org hkps://pgp.mit.edu; do
  if gpg --batch --keyserver "${ks}" --recv-keys "${FFMPEG_GPG_KEY}"; then
    gpg_recv_ok=1
    break
  fi
  echo "ffmpeg-build: keyserver ${ks} failed, trying next" >&2
done
if [ "${gpg_recv_ok}" -ne 1 ]; then
  echo "ffmpeg-build: could not retrieve FFmpeg signing key ${FFMPEG_GPG_KEY} from any keyserver." >&2
  exit 3
fi
gpg --batch --verify "${TARBALL}.asc" "${TARBALL}"

tar -xJf "${TARBALL}"
SRC="${WORK}/ffmpeg-${FFMPEG_VERSION}"

# -----------------------------------------------------------------------------
# 2. nv-codec-headers (NVIDIA path only) — install the NVENC/NVDEC headers from
#    a content-pinned git commit. No CUDA SDK: --enable-cuda-llvm JIT-compiles
#    scale_cuda via clang, keeping the build LGPL-clean (ADR-0031, §4 brief).
# -----------------------------------------------------------------------------
if [ "${FF_HWACCEL}" = "nvidia" ]; then
  git clone https://github.com/FFmpeg/nv-codec-headers.git nv-codec-headers
  git -C nv-codec-headers checkout --quiet "${NVCODEC_COMMIT}"
  make -C nv-codec-headers install "PREFIX=${FF_PREFIX}"
fi

# -----------------------------------------------------------------------------
# 3. Configure — the EXACT ADR-0031 LGPL-clean allowlist. Built once here so the
#    flag list is reviewed in one place. Profiles diverge ONLY in the hwaccel
#    block (nvidia vs vaapi) and the opt-in GPL tail.
# -----------------------------------------------------------------------------
# Common (license-agnostic, hwaccel-agnostic) flags — ADR-0031 §"Enable-flags".
# shellcheck disable=SC2054  # the commas below are FFmpeg list-value syntax
# (e.g. --enable-decoder=h264,hevc,...), NOT array-element separators: each
# `--enable-X=a,b,c` is a single shell word / one array element (verified).
CONFIGURE_ARGS=(
  "--prefix=${FF_PREFIX}"
  --disable-everything
  --enable-shared --disable-static --enable-version3
  --disable-doc --disable-ffplay --disable-debug --disable-autodetect
  # ffprobe is KEPT (invariant #8: verify output with ffprobe); ffplay dropped.
  --enable-decoder=h264,hevc,av1,vp9,mpeg2video,mjpeg,ffv1,aac,opus,mp2,ac3,pcm_s16le
  --enable-encoder=mpeg2video,ffv1,mjpeg,rawvideo,aac,libopus,mp2,flac,pcm_s16le
  --enable-parser=h264,hevc,av1,vp9,aac,opus,mpegaudio,mpegvideo
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata,aac_adtstoasc,null
  --enable-demuxer=mpegts,hls,mov,flv,rtsp,rtp,sdp,aac,matroska,h264,hevc,data
  --enable-muxer=mpegts,mp4,mov,flv,rtsp,rtp,adts,data
  --enable-protocol=file,pipe,tcp,udp,rtp,rtsp,rtmp,hls,https,tls,crypto,data,srt
  --enable-filter=scale,format,aformat,aresample,anull,null,fps,setpts,asetpts
  --enable-swscale --enable-swresample --enable-avfilter
  --enable-libopus --enable-libsrt --enable-gnutls
)

# Hardware acceleration — both branches are LGPL-clean.
case "${FF_HWACCEL}" in
  nvidia)
    # NVENC/NVDEC/cuvid via nv-codec-headers; scale_cuda via cuda-llvm (no SDK).
    # --enable-nvenc/nvdec/cuvid turn on the CAPABILITY only; --disable-everything
    # still strips the actual encoders/decoders/hwaccels/filters unless each is
    # named in an allowlist below. All FFmpeg-8.1.1 names verified against the
    # n8.1.1 source (libavcodec/allcodecs.c, libavcodec/hwaccels.h,
    # libavfilter/allfilters.c). NVENC/NVDEC/cuvid/scale_cuda are all LGPL-clean.
    # shellcheck disable=SC2054  # commas are FFmpeg list-value syntax (one word)
    CONFIGURE_ARGS+=(
      --enable-ffnvcodec --enable-nvenc --enable-nvdec --enable-cuvid
      --enable-cuda-llvm
      --enable-encoder=h264_nvenc,hevc_nvenc
      --enable-decoder=h264_cuvid,hevc_cuvid,av1_cuvid,vp9_cuvid,mpeg2_cuvid
      --enable-hwaccel=h264_nvdec,hevc_nvdec,av1_nvdec,vp9_nvdec
      --enable-filter=scale_cuda
    )
    ;;
  vaapi)
    # Intel/AMD on Linux. libvpl (Intel QSV via oneVPL) is amd64-only.
    # --enable-vaapi turns on the CAPABILITY only; --disable-everything still
    # strips the VAAPI encoders/hwaccels/scale filter unless each is named in an
    # allowlist below. Names verified against FFmpeg n8.1.1 source
    # (libavcodec/allcodecs.c, libavcodec/hwaccels.h, libavfilter/allfilters.c).
    # VAAPI encode/decode is LGPL-clean.
    # shellcheck disable=SC2054  # commas are FFmpeg list-value syntax (one word)
    CONFIGURE_ARGS+=(
      --enable-vaapi --enable-libdrm
      --enable-encoder=h264_vaapi,hevc_vaapi,mjpeg_vaapi
      --enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi,vp9_vaapi
      --enable-filter=scale_vaapi
    )
    if [ "${TARGETARCH}" = "amd64" ]; then
      CONFIGURE_ARGS+=(--enable-libvpl)
    fi
    ;;
  *)
    echo "ffmpeg-build: unknown FF_HWACCEL='${FF_HWACCEL}' (want nvidia|vaapi)" >&2
    exit 2
    ;;
esac

# Licensing profile. lgpl = default (NO gpl/nonfree). gpl = opt-in, `-gpl` tag.
case "${FF_LICENSE}" in
  lgpl)
    : # nothing added — the allowlist above is already LGPL-clean.
    ;;
  gpl)
    CONFIGURE_ARGS+=(
      --enable-gpl
      --enable-libx264 --enable-encoder=libx264
      --enable-libx265 --enable-encoder=libx265
    )
    ;;
  *)
    echo "ffmpeg-build: unknown FF_LICENSE='${FF_LICENSE}' (want lgpl|gpl)" >&2
    exit 2
    ;;
esac

# Hard guardrail: a NONfree or (unless explicitly FF_LICENSE=gpl) GPL flag must
# NEVER reach configure. Catches an accidental flag leak before it taints the .so.
for arg in "${CONFIGURE_ARGS[@]}"; do
  case "${arg}" in
    --enable-nonfree|--enable-libnpp|--enable-cuda-nvcc|--enable-libfdk?aac|--enable-openssl)
      echo "ffmpeg-build: BANNED flag '${arg}' in configure args — aborting (LGPL/redistributable guardrail)." >&2
      exit 4
      ;;
    --enable-gpl|--enable-libx264|--enable-libx265)
      if [ "${FF_LICENSE}" != "gpl" ]; then
        echo "ffmpeg-build: GPL flag '${arg}' present but FF_LICENSE=${FF_LICENSE} — aborting (LGPL default must stay GPL-free)." >&2
        exit 4
      fi
      ;;
  esac
done

# -----------------------------------------------------------------------------
# 4. Build + install. PKG_CONFIG_PATH lets configure see the nv-codec-headers
#    .pc files installed into the prefix above (NVIDIA path).
# -----------------------------------------------------------------------------
cd "${SRC}"
export PKG_CONFIG_PATH="${FF_PREFIX}/lib/pkgconfig:${PKG_CONFIG_PATH:-}"

echo "ffmpeg-build: ./configure ${CONFIGURE_ARGS[*]}"
./configure "${CONFIGURE_ARGS[@]}"
make -j"$(nproc)"
make install

# -----------------------------------------------------------------------------
# 5. Assert the FFmpeg 8.1.1 soname landed (libavcodec.so.62, NOT .61). A layout
#    or version drift fails the BUILD here, not the deploy.
# -----------------------------------------------------------------------------
test -e "${FF_PREFIX}/lib/libavcodec.so.62"
test -e "${FF_PREFIX}/lib/libavutil.so.60"
test -e "${FF_PREFIX}/lib/libavformat.so.62"
echo "ffmpeg-build: OK — FFmpeg ${FFMPEG_VERSION} installed to ${FF_PREFIX} (libavcodec.so.62)."
