#!/usr/bin/env bash
#
# deploy/ffmpeg-build.sh — build Multiview's OWN pinned FFmpeg from source.
#
# Ratifies ADR-0031 (FF-0b): replace the third-party libav* (jellyfin-ffmpeg7 on
# the NVIDIA image, Debian-trixie apt libav*-dev on the VAAPI image) with a
# pinned, GPG- AND SHA-256-verified FFmpeg 9.0.2 source build under OUR exact
# `--disable-everything` allowlist. This is the single, audited source of the
# configure flag list — both deploy/Dockerfile and deploy/Dockerfile.nvidia call
# this one script, so the LGPL-clean guardrail (no GPL/nonfree leak) lives in
# exactly one place.
#
# Soname target: FFmpeg 9.0.2 → libavcodec.so.63 / libavutil.so.61 /
# libavformat.so.63 / libavfilter.so.12 / libswscale.so.10 / libswresample.so.7.
# This matches `ffmpeg-sys-next 9.0.1` (build.rs maps `ffmpeg_9_0` → libavcodec
# 63), the version the workspace lockfile resolves, so `cargo build` links .so.63.
#
# Inputs (environment variables, supplied by the Dockerfile via ARG→ENV):
#   FFMPEG_VERSION   FFmpeg release to build               (default 9.0.2)
#   FFMPEG_SHA256    SHA-256 of ffmpeg-${FFMPEG_VERSION}.tar.xz  (REQUIRED)
#   FFMPEG_GPG_KEY   FFmpeg release signing key fingerprint (default below)
#   FFMPEG_GIT_COMMIT upstream commit of tag n${FFMPEG_VERSION}; fallback source
#                    used only when ffmpeg.org is unreachable (content-pinned)
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
#   SHA256 8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
#          (independently confirmed: Fossies SHA-256, rpmfusion SHA-512).
#
# LGPL-clean guardrail (ADR-0031 §"Enable-flags", CLAUDE.md §7): the default
# profile passes NO --enable-gpl, NO --enable-nonfree, NO --enable-libnpp,
# --enable-cuda-nvcc, --enable-libx264/265, --enable-libfdk_aac, --enable-openssl.
# NVENC/NVDEC/cuvid/cuda-llvm/vaapi/libvpl/gnutls/libsrt/libopus are all
# LGPL-clean. We scale with scale_cuda (--enable-cuda-llvm), never scale_npp.
set -euo pipefail
# Resolve this script's own directory NOW, before any `cd`: a relative $0
# (e.g. `bash deploy/ffmpeg-build.sh`) stops resolving once the build cds into
# its temp dir, which used to break the fetch-script and patch-dir lookups.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"

FFMPEG_VERSION="${FFMPEG_VERSION:-9.0.2}"
FFMPEG_GPG_KEY="${FFMPEG_GPG_KEY:-FCF986EA15E6E293A5644F10B4322F04D67658D8}"
FF_LICENSE="${FF_LICENSE:-lgpl}"
FF_HWACCEL="${FF_HWACCEL:-vaapi}"
FF_PREFIX="${FF_PREFIX:-/opt/multiview-ffmpeg}"
NVCODEC_COMMIT="${NVCODEC_COMMIT:-e844e5b26f46bb77479f063029595293aa8f812d}"
TARGETARCH="${TARGETARCH:-amd64}"
# Host OS: Linux builds the container/release prefix; Darwin builds the native
# macOS prefix (FF_HWACCEL=apple). Shared libraries are named per platform.
HOST_OS="$(uname -s)"
cpu_count() { getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4; }
# The installed major-versioned library for a libav component, e.g.
# soname avcodec 63 -> libavcodec.so.63 (Linux) / libavcodec.63.dylib (macOS).
soname() {
  if [ "${HOST_OS}" = "Darwin" ]; then echo "lib$1.$2.dylib"; else echo "lib$1.so.$2"; fi
}
# Run a prefix binary with the prefix's libraries on the loader path.
with_prefix_libs() {
  if [ "${HOST_OS}" = "Darwin" ]; then
    DYLD_LIBRARY_PATH="${FF_PREFIX}/lib:${DYLD_LIBRARY_PATH:-}" "$@"
  else
    LD_LIBRARY_PATH="${FF_PREFIX}/lib:${LD_LIBRARY_PATH:-}" "$@"
  fi
}

# Run this independently in the image builder AND after installation, so a
# cached build cannot publish an FFmpeg lacking the promised input/output IO.
assert_features() {
  local ffmpeg="${FF_PREFIX}/bin/ffmpeg" listing kind name
  for kind in protocols demuxers muxers; do
    listing="$(with_prefix_libs "${ffmpeg}" -hide_banner "-${kind}")"
    case "${kind}" in
      protocols)
        # SRT is bidirectional. RTSP and HLS are *demuxers*, not protocols.
        if [ "$(printf '%s\n' "${listing}" | awk '$1 == "srt" { count++ } END { print count+0 }')" -ne 2 ]; then
          echo "ffmpeg-build: missing input/output protocol srt" >&2
          return 1
        fi
        ;;
      demuxers)
        for name in rtsp hls wav f32le s16le s24le; do
          if ! printf '%s\n' "${listing}" | awk -v n="${name}" '$1 == "D" { split($2, aliases, ","); for (i in aliases) if (aliases[i] == n) found=1 } END { exit !found }'; then
            echo "ffmpeg-build: missing demuxer ${name}" >&2
            return 1
          fi
        done
        ;;
      muxers)
        for name in rtsp mpegts mp4 wav f32le s16le s24le; do
          if ! printf '%s\n' "${listing}" | awk -v n="${name}" '$1 == "E" { split($2, aliases, ","); for (i in aliases) if (aliases[i] == n) found=1 } END { exit !found }'; then
            echo "ffmpeg-build: missing muxer ${name}" >&2
            return 1
          fi
        done
        ;;
    esac
  done
  echo "ffmpeg-build: required protocols, demuxers and muxers present."
}

# Exercise actual SRT transport, not merely ffmpeg -protocols membership. This
# runs in the image builder for every licensing/hardware variant, without access
# to any network other than loopback.
smoke_srt() (
  local ffmpeg="${FF_PREFIX}/bin/ffmpeg" host port work tmp_root listener_pid='' pushed=0 attempt ready tick udp_table port_hex previous_port=''
  tmp_root="$(realpath -- "${TMPDIR:-/tmp}")"
  work="$(mktemp -d "${tmp_root}/ffmpeg-srt-smoke.XXXXXXXX")"
  # The only path removed below is this mktemp directory under the verified root.
  if [[ "$(realpath -- "${work}")" != "${tmp_root}/ffmpeg-srt-smoke."* ]]; then
    echo 'ffmpeg-build: unexpected SRT smoke directory' >&2
    return 1
  fi
  trap 'if [[ -n "${listener_pid:-}" ]]; then kill "${listener_pid}" 2>/dev/null || :; wait "${listener_pid}" 2>/dev/null || :; fi; rm -rf -- "${work}"' EXIT

  # Some builders disable IPv6 loopback; only those fall back to IPv4.
  host='[::1]'
  if ! grep -q '^00000000000000000000000000000001 ' /proc/net/if_inet6 2>/dev/null; then
    host='127.0.0.1'
    echo 'ffmpeg-build: IPv6 loopback unavailable; using 127.0.0.1' >&2
  fi
  printf 'Multiview FFmpeg SRT loopback smoke: 9.0.2\n' > "${work}/in.bin"
  if [[ "${host}" == '[::1]' ]]; then udp_table=/proc/net/udp6; else udp_table=/proc/net/udp; fi

  # Each caller needs its own listener: a failed/slow caller can consume the
  # listener's only accept, and retrying against that listener cannot recover.
  # Under heavy shared-host load, SRT connect/close occasionally stalled despite
  # a confirmed UDP bind; its precise cause was not established. Keep retries as
  # a bounded backstop, not a substitute for waiting until the listener binds.
  for attempt in 1 2 3; do
    # Avoid the Linux ephemeral-port range (typically starting at 32768).
    port=$((20000 + RANDOM % 10000))
    while [[ "${port}" == "${previous_port}" ]]; do port=$((20000 + RANDOM % 10000)); done
    previous_port="${port}"
    port_hex="$(printf '%04X' "${port}")"
    # File-transfer mode drains buffered bytes on close; live mode can discard
    # a short payload when the sender exits before SRT's default latency.
    LD_LIBRARY_PATH="${FF_PREFIX}/lib:${LD_LIBRARY_PATH:-}" timeout -k 2s 45s \
      "${ffmpeg}" -hide_banner -loglevel error -nostdin -y \
      -f data -i "srt://${host}:${port}?mode=listener&transtype=file" \
      -map 0 -c copy -f data "${work}/out.bin" > "${work}/listener.log" 2>&1 &
    listener_pid=$!
    # Poll the actual UDP bind rather than sleeping a guessed startup delay.
    # 150 * 0.1s bounds readiness to 15 seconds per attempt.
    ready=0
    for ((tick = 0; tick < 150; tick++)); do
      if awk -v p=":${port_hex}" '$2 ~ (p "$") && $4 == "07" { found=1; exit } END { exit !found }' "${udp_table}"; then
        ready=1
        break
      fi
      if ! kill -0 "${listener_pid}" 2>/dev/null; then break; fi
      sleep 0.1
    done
    if [[ "${ready}" -eq 1 ]] &&
      LD_LIBRARY_PATH="${FF_PREFIX}/lib:${LD_LIBRARY_PATH:-}" timeout -k 2s 20s \
        "${ffmpeg}" -hide_banner -loglevel error -nostdin \
        -f data -i "${work}/in.bin" -map 0 -c copy -f data \
        "srt://${host}:${port}?mode=caller&transtype=file" > "${work}/caller.log" 2>&1; then
      pushed=1
      break
    fi
    echo "ffmpeg-build: SRT attempt ${attempt} failed; restarting listener" >&2
    kill "${listener_pid}" 2>/dev/null || :
    wait "${listener_pid}" 2>/dev/null || :
    listener_pid=''
  done
  if [ "${pushed}" -ne 1 ]; then
    echo 'ffmpeg-build: SRT caller failed' >&2
    cat "${work}/listener.log" "${work}/caller.log" >&2
    return 1
  fi
  if ! wait "${listener_pid}"; then
    listener_pid=''
    echo 'ffmpeg-build: SRT listener failed or timed out' >&2
    cat "${work}/listener.log" "${work}/caller.log" >&2
    return 1
  fi
  listener_pid=''
  if ! cmp -s "${work}/in.bin" "${work}/out.bin"; then
    echo 'ffmpeg-build: SRT loopback payload mismatch' >&2
    cat "${work}/listener.log" "${work}/caller.log" >&2
    return 1
  fi
  echo "ffmpeg-build: SRT loopback push+ingest PASS (${host})."
)

case "${1:-}" in
  --assert-features) assert_features; exit ;;
  --smoke-srt) smoke_srt; exit ;;
esac

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
# 1. Verified source. The Docker build pre-fetches it in a separate cached stage
#    (FFMPEG_SRC_DIR); standalone runs fetch it here. Both paths go through
#    ffmpeg-fetch-source.sh: SHA-256 pin AND GPG signature, or the pinned
#    upstream git commit when ffmpeg.org is unreachable.
# -----------------------------------------------------------------------------
SRC="${WORK}/ffmpeg-${FFMPEG_VERSION}"
if [ -n "${FFMPEG_SRC_DIR:-}" ]; then
  cp -a "${FFMPEG_SRC_DIR}/ffmpeg-${FFMPEG_VERSION}" "${SRC}"
else
  "${SCRIPT_DIR}/ffmpeg-fetch-source.sh" "${WORK}"
fi

# -----------------------------------------------------------------------------
# 1b. Carried patches for this exact version (deploy/ffmpeg-patches/<version>/,
#     applied in name order). Each is a minimal fix for a regression we hit,
#     documented in its header with the upstream status; the directory is part of
#     the build input, so a patch change rebuilds the base image. A patch that no
#     longer applies fails the build rather than being skipped.
# -----------------------------------------------------------------------------
PATCH_DIR="${FFMPEG_PATCH_DIR:-${SCRIPT_DIR}/ffmpeg-patches}/${FFMPEG_VERSION}"
if [ -d "${PATCH_DIR}" ]; then
  for p in "${PATCH_DIR}"/*.patch; do
    [ -e "${p}" ] || continue
    echo "ffmpeg-build: applying $(basename "${p}")"
    patch -d "${SRC}" -p1 --forward --batch < "${p}"
  done
fi

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
  --enable-decoder=h264,hevc,av1,vp9,mpeg2video,mjpeg,ffv1,aac,opus,mp2,ac3,pcm_s16le,pcm_s24le,pcm_f32le,flac
  # Caption/subtitle decoders the caption pipeline opens by name
  # (multiview-ffmpeg caption_decode: cc_dec=ccaption, webvtt, subrip, mov_text=movtext, ass).
  --enable-decoder=ccaption,webvtt,subrip,movtext,ass
  --enable-encoder=mpeg2video,ffv1,mjpeg,rawvideo,aac,libopus,mp2,flac,pcm_s16le,pcm_s24le,pcm_f32le
  --enable-parser=h264,hevc,av1,vp9,aac,opus,mpegaudio,mpegvideo
  # dump_extradata (runtime name dump_extra): in-band SPS/PPS before every IDR for
  # continuous-ES splicing (multiview-ffmpeg bsf_select FILTER_DUMP_EXTRA).
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata,dump_extradata,aac_adtstoasc,null
  --enable-demuxer=mpegts,hls,mov,flv,rtsp,rtp,sdp,aac,matroska,h264,hevc,data,wav,pcm_f32le,pcm_s16le,pcm_s24le,flac,ogg,webvtt
  --enable-muxer=mpegts,mp4,mov,flv,rtsp,rtp,adts,data,wav,pcm_f32le,pcm_s16le,pcm_s24le,flac,matroska
  # RTSP/HLS are demuxers (RTSP also a muxer); libsrt is the SRT URL protocol.
  --enable-protocol=file,pipe,tcp,udp,rtp,rtmp,http,https,tls,crypto,data,libsrt
  --enable-filter=scale,format,aformat,aresample,anull,null,fps,setpts,asetpts
  --enable-swscale --enable-swresample --enable-avfilter
  --enable-libopus --enable-libsrt --enable-gnutls
  # zlib: HTTP gzip Content-Encoding — some HLS origins gzip their playlists;
  # without it libav fails the fetch ("need zlib with gzip support"). zlib is
  # zlib-licensed (permissive), so the build stays LGPL-clean.
  --enable-zlib
)

# Hardware acceleration — both branches are LGPL-clean.
case "${FF_HWACCEL}" in
  nvidia)
    # NVENC/NVDEC/cuvid via nv-codec-headers; scale_cuda via cuda-llvm (no SDK).
    # --enable-nvenc/nvdec/cuvid turn on the CAPABILITY only; --disable-everything
    # still strips the actual encoders/decoders/hwaccels/filters unless each is
    # named in an allowlist below. All FFmpeg-9.0.2 names verified against the
    # n9.0.2 source (libavcodec/allcodecs.c, libavcodec/hwaccels.h,
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
    # allowlist below. Names verified against FFmpeg n9.0.2 source
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
  apple)
    # Native macOS (ADR-0011): VideoToolbox encode/decode + AudioToolbox, all
    # Apple system frameworks — LGPL-clean (no --enable-gpl/nonfree). Built
    # natively on a Mac; containers cannot reach VideoToolbox. Names verified
    # against the n9.0.2 source (libavcodec/allcodecs.c, hwaccels.h,
    # libavfilter/allfilters.c). avfoundation is the macOS capture indev.
    if [ "${HOST_OS}" != "Darwin" ]; then
      echo "ffmpeg-build: FF_HWACCEL=apple must be built on macOS" >&2
      exit 2
    fi
    # shellcheck disable=SC2054  # commas are FFmpeg list-value syntax (one word)
    CONFIGURE_ARGS+=(
      --enable-videotoolbox --enable-audiotoolbox
      --enable-encoder=h264_videotoolbox,hevc_videotoolbox,prores_videotoolbox
      --enable-hwaccel=h264_videotoolbox,hevc_videotoolbox,vp9_videotoolbox,av1_videotoolbox,prores_videotoolbox
      --enable-filter=scale_vt
      --enable-avfoundation --enable-indev=avfoundation --enable-avdevice
      --enable-encoder=aac_at --enable-decoder=aac_at
    )
    ;;
  *)
    echo "ffmpeg-build: unknown FF_HWACCEL='${FF_HWACCEL}' (want nvidia|vaapi|apple)" >&2
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
# FFmpeg accepts unknown names in --enable-protocol=... silently. Check the
# resolved components now (before the expensive compile) as well as the final
# CLI below. The URL scheme srt is implemented by CONFIG_LIBSRT_PROTOCOL.
for component in LIBSRT_PROTOCOL RTSP_DEMUXER HLS_DEMUXER WAV_DEMUXER \
                 CCAPTION_DECODER WEBVTT_DECODER SUBRIP_DECODER MOVTEXT_DECODER ASS_DECODER \
                 DUMP_EXTRADATA_BSF OGG_DEMUXER WEBVTT_DEMUXER MATROSKA_MUXER \
                 PCM_F32LE_DEMUXER PCM_S16LE_DEMUXER PCM_S24LE_DEMUXER \
                 WAV_MUXER PCM_F32LE_MUXER PCM_S16LE_MUXER PCM_S24LE_MUXER; do
  if ! awk -v n="CONFIG_${component}" '$1 == "#define" && $2 == n && $3 == "1" { found=1 } END { exit !found }' config_components.h; then
    echo "ffmpeg-build: requested ${component} was not enabled by configure" >&2
    exit 5
  fi
done
make -j"$(cpu_count)"
make install

# Licence texts travel WITH the libraries: every artifact that conveys these
# libav* binaries (app images, release tarballs) copies ${FF_PREFIX}/share/licenses.
# --enable-version3 makes the LGPL build LGPL-3.0-or-later, which requires the
# LGPLv3 AND GPLv3 texts; the gpl flavour is GPL-3.0-or-later. Ship all four from
# the verified source tree (FFmpeg's LICENSE.md names them) plus LICENSE.md.
install -d "${FF_PREFIX}/share/licenses/ffmpeg"
install -m 0644 LICENSE.md COPYING.LGPLv3 COPYING.GPLv3 COPYING.LGPLv2.1 COPYING.GPLv2 \
  "${FF_PREFIX}/share/licenses/ffmpeg/"

# -----------------------------------------------------------------------------
# 5. Assert actual installed features (configure flags alone do not prove they
#    survived dependency resolution) and the FFmpeg 9.0.2 sonames. Fail the BUILD
#    here rather than shipping an unusable image.
# -----------------------------------------------------------------------------
test -e "${FF_PREFIX}/lib/$(soname avcodec 63)"
test -e "${FF_PREFIX}/lib/$(soname avutil 61)"
test -e "${FF_PREFIX}/lib/$(soname avformat 63)"
test -s "${FF_PREFIX}/share/licenses/ffmpeg/COPYING.LGPLv3"
test -s "${FF_PREFIX}/share/licenses/ffmpeg/COPYING.GPLv3"
assert_features
echo "ffmpeg-build: OK — FFmpeg ${FFMPEG_VERSION} installed to ${FF_PREFIX} ($(soname avcodec 63))."
