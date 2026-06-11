# Example & Test Streams

A curated catalog of synthetic and license-clean public sources for **developing, demoing, and
testing** Multiview. These double as a deliberately diverse **gotcha test matrix** — between them
they exercise mismatched frame rates (25 / 29.92 / 29.97 / 30 / 50 / 60 fps), multiple codecs
(H.264, HEVC), tagged vs untagged color, an oddball mixed-primaries tag, three audio sample rates
(22.05 / 44.1 / 48 kHz), WebVTT subtitles, multivariant playlists, and both live and VOD.

> [!IMPORTANT]
> **The third-party public streams below are for testing/development only.** They are operated by
> others, may be rate-limited or **removed without notice**, and remain copyrighted by their
> owners — do not bundle, restream, or redistribute their content. The **[synthetic sources](#synthetic--reproducible-sources)**
> require no network and let you reproduce each failure mode deterministically, so prefer them for
> CI and for anything reproducible. The open-movie samples (Big Buck Bunny, Tears of Steel) are
> Blender Foundation Creative-Commons works and are fine to use.
>
> Stream properties can change at any time; re-verify with the [probe command](#re-verifying-a-stream).

---

## A diverse demo set

A good demo set differs on almost every axis (codec, fps, color tagging, audio rate) so the engine's
normalization is exercised end-to-end. The pattern below — two synthetic tiles, one example RTSP
camera, and one public open-movie sample — makes a self-contained 2×2 (it is what
[`examples/public-streams-2x2.toml`](../../examples/public-streams-2x2.toml) ships) and degrades
gracefully to a synthetic-only set with no network.

### 1. Synthetic untagged feed (25 fps PAL stand-in) — `bars` / lavfi

- **Source:** a built-in synthetic source (`bars`, of which `test` is a back-compat alias — see
  [ADR-0027](../decisions/ADR-0027.md)), or a lavfi `testsrc2` pushed into a local server (see the
  [synthetic recipes](#synthetic--reproducible-sources)).
- **Video:** H.264-class, `yuv420p`, **25 fps (PAL)** — renditions e.g. **1024×576**, **640×360**
- **Audio:** AAC-LC, **48 kHz**, stereo
- **Extras:** optional **WebVTT subtitles** when produced via a packager
- **Color:** **untagged** (no primaries/transfer/matrix/range signalled) → the *untagged-input* trap
- **Exercises:** multivariant rendition selection · 25 fps → output-cadence resampling · untagged
  color defaulting · subtitle ingest (WebVTT) · ABR ladder.

### 2. Synthetic BT.709-limited feed (29.97 fps NTSC stand-in) — `bars` / lavfi

- **Source:** a built-in synthetic source (`bars`; `test` is an alias — [ADR-0027](../decisions/ADR-0027.md)),
  or a lavfi `smptebars` at `rate=30000/1001` tagged BT.709.
- **Video:** H.264-class, `yuv420p`, **1280×720**, **29.97 fps (30000/1001)**
- **Audio:** AAC-LC, **48 kHz**, stereo
- **Color:** **fully tagged** — primaries `bt709`, transfer `bt709`, matrix `bt709`, range **`tv`** (limited)
- **Notes:** a steady, always-available reference tile — useful as a stable baseline against the
  PAL/60 sources.
- **Exercises:** fractional NTSC **29.97 fps** vs the PAL/60 sources · correctly-tagged **BT.709 limited**
  baseline · single-variant ingest.

### 3. Example RTSP camera (HEVC 1080p50) — RTSP, live

- **URL (example placeholder):** `rtsp://camera.example.net:8554/stream` (RFC-2606 example domain — swap
  for your own camera or a [local MediaMTX](#local-rtsp--srt--mpeg-ts-loopback-mediamtx) loopback)
- **Protocol:** RTSP (use **TCP** transport), live; e.g. served by an NVR / go2rtc
- **Video:** **HEVC (H.265) Main**, `yuv420p`, **1920×1080**, **50 fps**
- **Color:** primaries `bt709`, transfer `bt709`, matrix `bt709`, range **`tv`** (limited)
- **Notes:** RTSP has **no built-in jitter buffer**; reachability depends on the network/VPN. Prefer
  `rtsp_transport=tcp` for reliability over the public internet.
- **Exercises:** **HEVC** decode path · **50 fps** high-cadence source · 1080p tile downscale-on-decode
  (efficiency) · RTSP reconnect/supervision · TCP-vs-UDP transport.

---

## Public test streams

### HLS — VOD

| Name | URL | Video | FPS | Audio | Color | Notes |
|------|-----|-------|-----|-------|-------|-------|
| **Mux – Big Buck Bunny** | `https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8` | H.264 High, up to 1280×720 | **60** (ladder down to 30) | AAC **44.1 kHz** | untagged | hls.js reference stream; very stable. **44.1 kHz** audio → resample-to-48k test. |
| **Apple BipBop (TS)** | `https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_4x3/bipbop_4x3_variant.m3u8` | H.264 Main, 400×300 … 640×480 … | **≈29.92 (359/12)** | AAC **22.05 kHz** | **smpte170m** primaries+matrix, **bt709** transfer, `tv` | The canonical Apple test ladder. **Quirky mixed color tags** + **non-standard fps** + **22.05 kHz** → triple torture test. MPEG-TS segments. |
| **Apple BipBop (fMP4/CMAF)** | `https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8` | H.264 High, up to 1920×1080 | **60** | AAC 48 kHz | untagged | **fMP4/CMAF** segments + **WebVTT** subs → exercises the fMP4 ingest path and subtitle rendition selection. |
| **Akamai player samples** | `https://players.akamai.com/hls/` | BBB / Sintel / Tears of Steel | mixed | — | mixed | Page lists several stable VOD test streams (incl. 4K). Grab the `.m3u8` from the page. |

### HLS — Live (or live-like)

There is no need to depend on any specific broadcast feed. For an "always-on, multivariant, 30 fps,
untagged" live tile, run a synthetic source through a local packager (see the
[synthetic recipes](#synthetic--reproducible-sources)) or point the ingest at a VOD sample in
VOD-as-live mode. If you want a genuinely-public live HLS endpoint, the community catalogs below list
many — pick any that is license-clean for your use.

| Name | URL | Video | FPS | Color | Notes |
|------|-----|-------|-----|-------|-------|
| **Synthetic live (local)** | `http://[::1]:8888/test/index.m3u8` | H.264 Main, ABR to 576p | 25 / 30 | untagged or tagged | Produced by pushing a lavfi source into a [local MediaMTX](#local-rtsp--srt--mpeg-ts-loopback-mediamtx) / packager. Reproducible, multivariant, no third party. |
| **iptv-org** | `https://github.com/iptv-org/iptv` | — | — | — | Huge community M3U catalog of free FAST channels (thousands). Quality/uptime varies wildly — great for stress/fuzz testing ingest robustness. Check each channel's license/terms before use. |

### RTSP

| Name | URL | Notes |
|------|-----|-------|
| **Example camera (placeholder)** | `rtsp://camera.example.net:8554/stream` | RFC-2606 example domain — stands in for an HEVC 1080p50 NVR camera. See [above](#3-example-rtsp-camera-hevc-1080p50--rtsp-live). Swap for your own camera. |
| **Wowza RTSP test** | `https://www.wowza.com/developer/rtsp-stream-test` | Looping test clip. The actual `rtsp://…` URL is **per-session/dynamic** — copy it from that page at test time. |
| **rtsp.stream** | `https://rtsp.stream/` | Free public RTSP test service (sign up for a key; provides stable looping `rtsp://…` URLs). |
| **Local MediaMTX (recommended)** | `rtsp://[::1]:8554/test` | Most reliable for CI/dev — run your own. See [synthetic sources](#local-rtsp--srt--mpeg-ts-loopback-mediamtx). |

> Public, internet-reachable RTSP endpoints are rare and flaky. For anything reproducible, run a
> **local RTSP server** and push a synthetic source into it.

### MPEG-TS / SRT / UDP

Stable, public MPEG-TS-over-UDP/SRT endpoints essentially **do not exist** on the open internet
(UDP/multicast is operator-LAN only; most SRT demos are ephemeral). Generate them locally:

- **MPEG-TS over UDP:** `udp://[::1]:1234?pkt_size=1316` (see recipes below)
- **SRT (caller/listener):** `srt://[::1]:9000?mode=listener` ↔ `srt://[::1]:9000`
- `iptv-org` contains some `udp://`/`.ts` entries, but they are typically only reachable inside the
  originating operator network.

### NDI

There is **no public NDI over the internet** — NDI discovers senders via mDNS on the **local network**
and is bandwidth-heavy by design. To test NDI in/out:

- Install **NDI Tools** (free) and run **Test Pattern** (a sender) and **Studio Monitor** / **Video Monitor**
  (a receiver) on the same LAN.
- Or use the NDI SDK example senders/receivers.
- Multiview's NDI support is **feature-gated and runtime-optional** (proprietary SDK) — see the NDI docs.

---

## Gotcha coverage matrix

What each source lets you test. Aim to keep at least one stream per column in the dev/demo set.

| Stream | Codec | FPS | Color tags | Range | Audio Hz | Subs | Live | Multivariant |
|--------|-------|-----|-----------|-------|----------|------|------|--------------|
| Synthetic untagged 25p | H.264 | **25** | untagged | — | 48k | **WebVTT** | ✅ | ✅ |
| Synthetic BT.709 29.97p | H.264 | **29.97** | BT.709 | tv | 48k | — | ✅ | — |
| Example RTSP cam | **HEVC** | **50** | BT.709 | tv | — | — | ✅ | — |
| Mux BBB | H.264 | **60** | untagged | — | **44.1k** | — | — | ✅ |
| Apple BipBop TS | H.264 | **≈29.92** | **smpte170m/bt709 mix** | tv | **22.05k** | — | — | ✅ |
| Apple BipBop fMP4 | H.264 | **60** | untagged | — | 48k | **WebVTT** | — | ✅ |
| Synthetic live 30p | H.264 | 30 | untagged | — | 48k | — | ✅ | ✅ |

Coverage achieved: **fps** {25, 29.92, 29.97, 30, 50, 60} · **codecs** {H.264, HEVC} · **color**
{untagged, BT.709-limited, BT.601-primaries-with-709-transfer} · **audio** {22.05k, 44.1k, 48k} ·
**subtitles** {WebVTT} · **delivery** {live, VOD} · **playlist** {single, multivariant}.

---

## Synthetic & reproducible sources

For CI and for reproducing a specific failure mode **on demand and offline**. Multiview should ship
test fixtures built on these. (`testsrc2` = moving test pattern with timecode; `smptebars` = color
bars; `sine` = audio tone.)

> The lavfi recipes below produce **external** streams (with chosen fps/codec/color tags) to exercise
> the libav *decode* path. They are distinct from Multiview's **in-process** synthetic source kinds —
> `bars`, `solid`, and `clock` — which render in pure Rust with no libav and no subprocess
> ([ADR-0027](../decisions/ADR-0027.md)); see [`examples/synthetic-sources.toml`](../../examples/synthetic-sources.toml).
> Use the in-process kinds for a self-contained picture; use the recipes when you need a specific
> on-the-wire tagging/codec to test ingest.

### Generate sources with *exact* frame rate + color tags

```bash
# 25 fps PAL, UNTAGGED color (the untagged-defaulting trap) — tests untagged defaulting
ffmpeg -re -f lavfi -i "testsrc2=size=1024x576:rate=25" \
       -f lavfi -i "sine=frequency=1000:sample_rate=48000" \
       -c:v libx264 -profile:v main -pix_fmt yuv420p -g 50 \
       -c:a aac -ar 48000 -f mpegts "udp://[::1]:1234?pkt_size=1316"

# 29.97 fps, correctly tagged BT.709 LIMITED (a steady reference tile)
ffmpeg -re -f lavfi -i "smptebars=size=1280x720:rate=30000/1001" \
       -vf "format=yuv420p,scale=out_range=tv" \
       -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range tv \
       -c:v libx264 -profile:v main -g 60 -f mpegts "udp://[::1]:1235?pkt_size=1316"

# 30 fps tagged BT.601 (smpte170m) — deliberately DIFFERENT colorimetry to catch per-tile conversion bugs
ffmpeg -re -f lavfi -i "testsrc2=size=720x576:rate=30" \
       -vf "format=yuv420p,scale=out_range=tv" \
       -color_primaries smpte170m -color_trc smpte170m -colorspace smpte170m -color_range tv \
       -c:v libx264 -f mpegts "udp://[::1]:1236?pkt_size=1316"

# FULL-range source — catches limited<->full range handling (washed-out / crushed blacks)
ffmpeg -re -f lavfi -i "smptebars=size=1280x720:rate=25" \
       -vf "format=yuv420p,scale=out_range=pc" \
       -color_primaries bt709 -color_trc bt709 -colorspace bt709 -color_range pc \
       -c:v libx264 -f mpegts "udp://[::1]:1237?pkt_size=1316"

# 50 fps HEVC 1080p (the example RTSP camera stand-in) — tests the HEVC path + downscale-on-decode
ffmpeg -re -f lavfi -i "testsrc2=size=1920x1080:rate=50" \
       -c:v libx265 -pix_fmt yuv420p -f mpegts "udp://[::1]:1238?pkt_size=1316"
```

### Local RTSP / SRT / MPEG-TS loopback (MediaMTX)

[MediaMTX](https://github.com/bluenviron/mediamtx) is a single-binary server that ingests and
re-serves RTSP/RTMP/HLS/SRT/WebRTC — ideal for a reproducible local test rig.

```bash
# 1) Run the server (Docker)
docker run --rm -it --network=host bluenviron/mediamtx

# 2) Publish a synthetic source to it over RTSP
ffmpeg -re -f lavfi -i "testsrc2=size=1280x720:rate=30" \
       -f lavfi -i "sine=frequency=440:sample_rate=48000" \
       -c:v libx264 -preset veryfast -tune zerolatency -pix_fmt yuv420p \
       -c:a aac -ar 48000 -f rtsp "rtsp://[::1]:8554/test"

# 3) Consume it as RTSP / SRT / HLS:
#    rtsp://[::1]:8554/test
#    srt://[::1]:8890?streamid=read:test
#    http://[::1]:8888/test/index.m3u8
```

### Reproducing the HLS "bursting" failure

When ingesting live HLS, a player/ingester that downloads several buffered segments at once will
play them **too fast** unless paced to wall-clock by PTS. To reproduce: point the ingest at a VOD
HLS (e.g. Mux BBB) and observe it racing ahead with no pacer; then enable Multiview's input pacer and
confirm it locks to real-time. (See the streaming-gotchas runbook for the pacing design.)

---

## Re-verifying a stream

```bash
# Properties incl. color metadata (swap -rtsp_transport for RTSP sources)
ffprobe -v error \
  -show_entries 'stream=codec_type,codec_name,profile,width,height,r_frame_rate,avg_frame_rate,pix_fmt,color_primaries,color_transfer,color_space,color_range,sample_rate,channels:format=format_name' \
  -of json "<URL>"

# RTSP over TCP:
ffprobe -v error -rtsp_transport tcp -rw_timeout 12000000 -of json \
  -show_entries 'stream=codec_name,width,height,r_frame_rate,color_space,color_range' \
  "rtsp://host:8554/path"
```

A "what's wrong with my colors?" checklist and the full color-handling design live in the
color-management runbook (see `docs/`).
