# Local capture devices: cameras and capture cards (design note)

Status: proposed (macOS first; Windows next).

## Goal

Ingest a camera or capture card attached to the host as a normal source tile: macOS
AVFoundation, Windows DirectShow (Media Foundation later). Linux V4L2 follows the same shape.

## Decision

A new source kind `device`:

```toml
[[sources]]
id = "desk-cam"
kind = "device"
backend = "avfoundation"       # avfoundation | dshow (| v4l2 later)
device = "0"                   # backend index or exact device name
# optional, passed to the device demuxer:
# size = "1920x1080"
# fps = "30000/1001"
# pixel_format = "nv12"
```

- **Ingest path:** the device is opened through our pinned LGPL libavdevice (`avfoundation` /
  `dshow` input formats, both in the platform FFmpeg profiles) with the SAME demuxer → decoder →
  last-good tile store → supervised reconnect as every network source. Nothing new sits on the
  output data plane, so invariants #1 (the clock never waits for a device), #3 (PTS from tick) and
  #10 (bounded queues) hold unchanged. A device that stops (unplugged, TCC revoked, busy) behaves
  exactly like a dead network source: the tile holds last-good, then NO SIGNAL, and the ingest loop
  retries with backoff.
- **Why libavdevice, not a native AVFoundation binding first:** the decode/normalise path already
  exists and is tested; libavdevice's avfoundation/dshow demuxers are LGPL and in the pinned build.
  A native binding (zero-copy CVPixelBuffer → IOSurface → Metal) is a later optimisation, behind
  the same config.
- **Open is bounded:** the device open runs with the existing interrupt deadline (`rw_timeout`),
  because a TCC prompt or a busy device can block indefinitely. A blocked open never reaches the
  data plane.
- **Permission model (macOS):** camera and microphone access is granted per *responsible* app by
  TCC. A process started from SSH or a daemon without a GUI session is always denied. Deployment
  runs the engine as a LaunchAgent in the user's GUI session with a stable code signature, so the
  grant persists across upgrades. A denied open surfaces as an ingest error naming TCC, not a
  silent black tile.
- **Device naming:** indexes are unstable across reboots/plugging; exact names are accepted and
  preferred. `multiview devices` (later) lists them.
- **Feature gate:** `local-capture` (off by default; on in the `apple` preset). Without it, a
  `device` source fails validation with a clear message.

## Evidence required

- Unit: config parse/validate (backend/device/size/fps), URL/options lowering, platform gate.
- Chaos: a device open that blocks must time out without stalling the clock; a device that
  disappears mid-run degrades the tile to last-good/NO SIGNAL while output never falters.
- Hardware: on a macOS test host, a camera tile in a real run, output never faltered, ffprobe
  of the output, frame cadence from the tick counter.
