#!/bin/bash
# The caption source of run.sh's check (h) (#418): a file with video (160x90 testsrc), an 880 Hz AAC tone and a
# subrip subtitle stream with one cue "CAP <n> T=<s>" at s = 2n seconds of the file (1.5 s on screen).
#   gen-captions.sh <out.mkv> [seconds, default 1500]
# A file source plays once and then holds its last frame, so the file must outlast the run.
set -euo pipefail
out="${1:?out.mkv}"; secs="${2:-1500}"; FFMPEG="${FFMPEG:-/usr/bin/ffmpeg}"
srt="${out%.*}.srt"
awk -v n="$((secs / 2))" 'function t(ms) { return sprintf("%02d:%02d:%02d,%03d", ms / 3600000, ms / 60000 % 60, ms / 1000 % 60, ms % 1000) }
  BEGIN { for (i = 0; i < n; i++) printf "%d\n%s --> %s\nCAP %d T=%d\n\n", i + 1, t(2000 * i), t(2000 * i + 1500), i, 2 * i }' > "$srt"
exec "$FFMPEG" -hide_banner -loglevel error -nostdin -y \
  -f lavfi -i "testsrc=size=160x90:rate=10:duration=$secs" -f lavfi -i "sine=frequency=880:sample_rate=48000:duration=$secs" -i "$srt" \
  -map 0:v -map 1:a -map 2:s -c:v libx264 -preset ultrafast -g 20 -pix_fmt yuv420p -c:a aac -b:a 48k -ac 2 \
  -c:s srt -metadata:s:s:0 language=eng "$out"
