#!/bin/bash
# Live HLS test-stream generator (ffmpeg). usage: gen-stream.sh <variant> <outdir> <seconds>
#   reference : the engine's planned cast format (DESIGN.md section 1): H.264 High 1920x1080 30000/1001 no B-frames,
#               keyframe every 30 frames, AAC-LC 48 kHz stereo, HLS fMP4, video and audio in SEPARATE renditions,
#               2 s segments, 10-segment window, PDT, master with CODECS.
#   prod-ts   : the shape of the existing /hls/restream output that failed on the TV: 1.001 s MPEG-TS segments,
#               6-segment window, video and audio in separate TS renditions.
# Environment (variants of the reference, for format experiments): SEG (segment seconds, default 2), WINDOW (10),
#   KEEP=1 (do not clear the directory first), MUXED=1 (audio muxed into the video rendition), PDT=0 (no EXT-X-PROGRAM-DATE-TIME), GEN (file-name generation
#   tag, default: epoch seconds; a restarted generator must not reuse segment names), VBR (video bit rate, 6M).
set -euo pipefail
variant="${1:?variant: reference|prod-ts}"; out="${2:?outdir}"; secs="${3:?seconds}"
FFMPEG="${FFMPEG:-/usr/bin/ffmpeg}"; gen="${GEN:-$(date +%s)}"; seg="${SEG:-2}"; window="${WINDOW:-10}"; vbr="${VBR:-6M}"
mkdir -p "$out"; cd "$out"
# A generator owns its directory: drop what a previous run left (playlists, segments, init files only).
# KEEP=1 keeps them: a restarted generator then replaces the playlists in place (no 404 in between).
[ "${KEEP:-0}" = "1" ] || find . -maxdepth 1 -type f \( -name '*.m3u8' -o -name '*.m3u8.tmp' -o -name '*.m4s' -o -name '*.mp4' -o -name '*.ts' -o -name '*.tmp' \) -delete
SRC=(-re -f lavfi -i "testsrc2=size=1920x1080:rate=30000/1001" -re -f lavfi -i "sine=frequency=440:sample_rate=48000")
ENC=(-c:v libx264 -preset veryfast -tune zerolatency -profile:v high -level 4.1 -pix_fmt yuv420p
     -b:v "$vbr" -maxrate "$vbr" -bufsize "$vbr" -g 30 -keyint_min 30 -sc_threshold 0 -bf 0
     -c:a aac -b:a 128k -ac 2 -ar 48000)
flags="delete_segments+independent_segments"; [ "${PDT:-1}" = "1" ] && flags="$flags+program_date_time"
case "$variant" in
  reference)
    if [ "${MUXED:-0}" = "1" ]; then
      exec "$FFMPEG" -hide_banner -loglevel error -nostdin "${SRC[@]}" -t "$secs" "${ENC[@]}" -map 0:v -map 1:a \
        -f hls -hls_time "$seg" -hls_list_size "$window" -hls_segment_type fmp4 -hls_flags "$flags" \
        -master_pl_name master.m3u8 -hls_segment_filename "seg-$gen-%05d.m4s" -hls_fmp4_init_filename "init-$gen.mp4" video.m3u8
    fi
    exec "$FFMPEG" -hide_banner -loglevel error -nostdin "${SRC[@]}" -t "$secs" "${ENC[@]}" -map 0:v -map 1:a \
      -f hls -hls_time "$seg" -hls_list_size "$window" -hls_segment_type fmp4 -hls_flags "$flags" \
      -var_stream_map "v:0,agroup:aud,name:video a:0,agroup:aud,default:yes,name:audio" -master_pl_name master.m3u8 \
      -hls_segment_filename "seg-%v-$gen-%05d.m4s" -hls_fmp4_init_filename "init-%v-$gen.mp4" '%v.m3u8' ;;
  prod-ts)
    exec "$FFMPEG" -hide_banner -loglevel error -nostdin "${SRC[@]}" -t "$secs" "${ENC[@]}" -map 0:v -map 1:a \
      -f hls -hls_time 1 -hls_list_size 6 -hls_flags "$flags" \
      -var_stream_map "v:0,agroup:aud,name:video a:0,agroup:aud,default:yes,name:audio" -master_pl_name master.m3u8 \
      -hls_segment_filename "%v-$gen-%05d.ts" '%v.m3u8' ;;
  *) echo "unknown variant $variant" >&2; exit 2 ;;
esac
