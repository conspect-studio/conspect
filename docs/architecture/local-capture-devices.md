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
- **Open is bounded:** see "Open is truly bounded" below. A blocked open never reaches the data
  plane.
- **Permission model (macOS):** camera and microphone access is granted per *responsible* app by
  TCC. A process started from SSH or a daemon without a GUI session is always denied. Deployment
  runs the engine as a LaunchAgent in the user's GUI session with a stable code signature, so the
  grant persists across upgrades. A denied open surfaces as an ingest error naming TCC, not a
  silent black tile.
- **Device naming:** indexes are unstable across reboots/plugging; exact names are accepted and
  preferred. `multiview devices` (later) lists them.
- **Availability:** no extra Cargo feature. A `device` source needs the backend's libavdevice
  format in the linked FFmpeg (the pinned `apple` / `windows` profiles carry `avfoundation` /
  `dshow`); when it is absent, the open fails with "input format is not available in the linked
  FFmpeg" and the tile stays NO SIGNAL — never a crash.

## Authorization

A `device` source opens this host's cameras and capture cards — the same trust class as a `file`
source reading a host path, and more sensitive (people in the room). The control API therefore
**refuses** `kind = "device"`: device sources are configured in the config file only, by whoever
already controls the host. Validation also refuses avfoundation's non-camera selectors — `none`
("no video device") and `Capture screen N` (screen capture, gated by a separate OS grant). Caveat:
avfoundation's numeric index range continues past the cameras into the screens, so an index beyond
the camera count still selects a screen; that is a config-file author's choice, still gated by the
OS Screen Recording grant. Prefer exact device names. "Device" here means a *local capture
device*; it is unrelated to the control plane's tally/sync/NMOS **device** resources.

## Open is truly bounded

libavdevice's avfoundation/dshow demuxers are `AVFMT_NOFILE`: they ignore `rw_timeout` and never
poll the interrupt callback during open. The open therefore runs on a helper thread and the ingest
thread waits at most 10 s; a stuck open is abandoned (the helper is reaped at exit) and the
supervisor retries, with at most one blocked open per device so a wedged device cannot accumulate
threads. While reading, a device that delivers no frame for 5 s is treated as failed so the
reconnect path runs (the demuxers report "no frame yet" as EAGAIN forever).

## Evidence required

- Unit: config parse/validate (backend/device/size/fps), URL/options lowering, platform gate.
- Chaos: a device open that blocks must time out without stalling the clock; a device that
  disappears mid-run degrades the tile to last-good/NO SIGNAL while output never falters.
- Hardware: on a macOS test host, a camera tile in a real run, output never faltered, ffprobe
  of the output, frame cadence from the tick counter.
