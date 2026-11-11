# Local audio devices (CoreAudio, WASAPI): design note

Status: design for [ADR-0106](../decisions/ADR-0106.md) (Proposed), issue #276. Class R3.

This note says how a local audio input device becomes a source and how program audio plays to a
local output device, and why neither can delay the output clock (invariant #1) or make the engine
wait on a device (invariant #10).

## 1. Threads and what each may touch

| Thread | Owner | May touch | Must never |
| ------ | ----- | --------- | ---------- |
| Device callback (OS real-time) | cpal | its own SPSC ring (push for input, pop for output); atomics for counters | lock, allocate, log, block, call into the engine |
| Input pump (normal priority) | `multiview-audio-device` | ring (pop), resampler, servo, `AudioStore::publish` | wait for the engine; wait without a bound for the device |
| Output drain (normal priority) | `multiview-audio-device` (a device-paced drain, section 3) | engine→drain SPSC ring (pop), resampler, servo, device ring (push) | wait on the engine |
| Lifecycle helper | `multiview-audio-device` | cpal open/close, with a deadline | run on the engine or pump thread |
| Output clock / program bus | engine | `AudioStore::read` (wait-free); engine→drain SPSC ring (wait-free push) | wait for a device, a ring, a lock or a lifecycle event |

All rings are preallocated `rtrb` SPSC rings. Push and pop are wait-free on both sides. No mutex is
shared between the engine and any device-side thread. (The HDMI `DisplayAudioPublisher` takes a
short mutex; this path deliberately does not reuse it.)

**Callback buffers.** The callback converts in fixed chunks of at most `CHUNK = 1024` frames
through a preallocated scratch of `CHUNK × channels` samples, or writes straight into ring slots
(`rtrb` write chunks). A callback that asks for more than `CHUNK` frames is served in several chunks.
It never resizes or allocates. A test-only global allocator counter proves zero allocations and zero
lock calls on the callback path.

## 2. Input path

```
device callback ──push──▶ [SPSC ring, ~200 ms] ──pop──▶ pump: map channels → resample (rate × servo ppm)
                                                          └──publish──▶ AudioStore::live (bounded, drop-oldest)
                                                                              ▲ wait-free read per tick
                                                                       output clock / program bus
```

- **Callback:** convert the device sample format to `f32` in place in a preallocated scratch
  buffer, then `push` as many frames as fit. If the ring is full, the rest is dropped and
  `ring_overflow_frames` is incremented (atomic). The callback never waits.
- **Fill signal:** the servo measures **unread** frames: `head_frame − read_cursor`, clamped to
  `0..=capacity`, through a new wait-free `AudioStore::unread_frames()`. (`buffered_frames` is the
  retained window length; it does not fall when the bus reads, so it cannot be the servo input.)
- **Rate:** the stream is opened at 48 kHz, the canonical rate. `AdaptiveResampler` is a
  same-rate time stretch (±5000 ppm at most), not a rate converter. A device that does not list
  48 kHz in its supported configs is refused with a latched warning (CoreAudio and WASAPI shared
  mode offer 48 kHz on the device classes we target). A fixed-ratio stage for other rates is a
  later, separate change.
- **Pump:** wakes every 5 ms (bounded sleep, stop-flag aware). It pops everything, maps the
  channels to the source layout (`ChannelMatrix`), and time-stretches with one
  `AdaptiveResampler`. The trim is the servo output: it holds `unread_frames` at the target
  (prebuffer + 20 ms) within ±500 ppm.
  - Sign: unread above target → emit fewer frames (negative ppm); below → more.
  - Limits: ±500 ppm steady clamp, the resampler's own ±5000 ppm hard clamp above it.
  - Reset: on open, reopen or a store re-sync (unread jumps by more than 100 ms), the servo
    integrator and the resampler phase reset, and the pump re-primes the prebuffer.
  - Result: a device crystal that is fast or slow against the output clock is absorbed, and the
    store neither drains nor overflows. A stall drains the cushion to silence (gap-free read);
    the burst after it is dropped down to the target by the store's drop-oldest bound.
- **Store:** `AudioStore::live(48 kHz, capacity ≈ 1 s, prebuffer ≈ 60 ms)`. A read is gap-free:
  silence for anything not written. A stalled or lost device reads as silence, and the picture and
  the tick do not change.

## 3. Output path

```
program bus ──push (wait-free)──▶ [engine→drain SPSC ring, ~200 ms] ──pop──▶ drain: servo + resampler
                                                                              └──push──▶ [device ring, ~60 ms] ──pop──▶ device callback
```

- **Engine side:** each tick pushes its program audio into the engine→drain ring. Wait-free. If the
  ring is full (the drain is stalled), the new block is dropped and `engine_drop_frames` counts it.
  The engine never takes a lock.
- **Drain** (a new loop in `multiview-audio-device`; it reuses `BufferServo`, `AdaptiveResampler`
  and `ChannelMatrix`, not the HDMI `drain_loop`, which depends on a PCM that blocks to pace it):
  - It is **device-paced**: it sleeps until the device ring's fill drops below its target, then
    tops the ring up to the target. The callback pop rate is the pace, so the drain cannot
    over-produce, whatever its own wake-up jitter.
  - The servo holds the engine→drain ring's fill near its target, so the engine clock and the
    device clock are matched within ±500 ppm (the sign and reset rules are those of section 2).
  - Partial pushes are kept for the next round. Nothing is dropped while the device consumes.
- **Callback:** pop up to the requested frames in `CHUNK`-sized pieces, convert to the device
  format, and fill any shortfall with silence. It counts `underrun_frames` and stores a monotonic
  heartbeat (callback count) in an atomic.
- **Distinct states,** each with its own transition:
  - *underrun* (callback found the ring short): counted; the servo re-anchors; no reopen.
  - *ring full* (device not consuming but heartbeat advancing): normal, the drain waits.
  - *stalled* (heartbeat has not advanced for 2 s while open) or *lost* (error callback): the drain
    leaves the steady loop, closes the stream through the lifecycle helper, resets both rings and
    the servo, and reopens with backoff.
- **Rates and channels:** the stream opens at 48 kHz. Channel negotiation is device-specific, not
  the HDMI ELD rule: the drain uses the device's own channel count from its supported configs
  (mono, stereo or multichannel) and folds or upmixes with `ChannelMatrix`. A device without a
  48 kHz config is refused with a latched warning.

## 4. Device lifecycle and errors

- **Discovery and identity.** `multiview devices audio` lists the host's input and output devices
  with cpal name, host API, the default flag, channel counts and supported rates. A device is
  configured by exact name, plus an optional `index` among devices with the same name (duplicates
  are numbered in OS enumeration order). `default` follows the OS default: when the OS default
  changes, the stream is reopened on the new default, and the change is logged.
- **One lifecycle helper per device**, a long-lived thread that owns the cpal `Stream`. Open and
  close requests go to it through a 1-slot channel (single flight). The requester waits at most
  2 s for the reply, then treats the attempt as failed and moves to backoff.
- **Late results.** A reply that arrives after its deadline carries the request's generation
  number. The helper drops any stream whose generation is not current, so a late successful open
  is closed, not used.
- **A stuck helper.** If the helper is still inside a cpal call when the next request is due, no
  new helper is started (the thread budget is one per device). The device goes to the latched
  `device_call_stuck` state, its source reads silence (input) or its output drops (output), and a
  warning is raised. When the call returns, the helper resumes and normal backoff continues.
  Teardown at shutdown waits at most 2 s per helper, then detaches it. The process exit takes it.
- The cpal error callback sets an atomic `lost` flag and records the error kind. It does nothing
  else. Stalled/lost detection is in sections 2 and 3. Reopen uses capped exponential backoff
  (250 ms → 10 s).
- Every warning is latched (logged once per state change), never per poll.

## 5. Permissions

- **macOS:** microphone use needs TCC consent for the responsible (top-level) binary. The shipped
  app bundle and launcher carry `NSMicrophoneUsageDescription` and a stable signing identity, so
  the grant survives updates. Without consent CoreAudio may deliver zeros or no callbacks.
  - *No callbacks* is detected as *stalled* (section 4) and reported as
    `stalled (permission or device)`.
  - *Exact zeros* are reported as `input_silent` only. Silence is never treated as proof of
    denial, because a muted or idle microphone gives the same signal.
  - Where the OS exposes it, the source also reads the authorization status
    (`AVCaptureDevice.authorizationStatus(for: .audio)` through cpal's dependencies or a small
    `objc2` call in the leaf crate), and reports `permission_denied` only when that says
    denied/restricted.
  - The operator runbook says how to grant or reset consent (System Settings → Privacy & Security
    → Microphone; `tccutil reset Microphone <bundle-id>`).
- **Windows:** the per-app microphone privacy setting. A denied open fails with an access error,
  which the source reports as `permission_denied` (with the setting's name in the message) and
  retries on the slow cadence.

## 6. Configuration and API

```toml
[[sources]]
id = "desk-mic"
kind = "audio_device"
device = "USB Advanced Audio Device"   # exact cpal device name, or "default"
channels = 1                           # optional; device default otherwise
# the stream always opens at 48 kHz (section 2)

[[outputs]]
type = "audio_device"
id = "control-room"
device = "default"
```

- Config validation rejects an empty name, a name with control characters, and `channels` 0 or
  above 32.
- **Admission rule (one place, every path).** Local devices are file-configured only. The control
  plane rejects `audio_device` (and capture `device`) sources and outputs in the single shared
  validation used by every mutation and import path: create, patch, replace, bulk add, config
  promotion/reversion and adoption of persisted desired state. Delete of a file-configured device
  resource is also refused. A remote client cannot cause a device open or reopen. Tests cover each
  route, and each resource id is still checked per object (BOLA).
- **Telemetry and alerts:** per device `state` (`open`, `stalled`, `lost`, `device_call_stuck`,
  `permission_denied`, `unsupported_rate`), `input_silent`, `ring_overflow_frames`,
  `underrun_frames`, `engine_drop_frames`, `reopens`, `servo_ppm` and `unread_ms`. Alerts: any
  state other than `open` for 10 s, and drops or underruns above 0.1 % of frames over 1 min.

## 7. Tests (failing first)

CI, no hardware, over a **simulated device**: a thread that calls the same callback closure on its
own clock.

1. Ring adapters: push/pop bounds, overflow and underrun counts, silence fill, chunking of a
   callback larger than `CHUNK`, and zero allocations and zero locks on the callback path
   (counting allocator).
2. Input servo: devices at +300 and −300 ppm against a simulated output clock for 10 simulated
   minutes keep `unread_frames` within ±10 ms of target.
3. Output drain: device-paced production matches the callback rate within ±0.1 % under random
   drain wake-up jitter (0–20 ms), and the engine→drain fill stays bounded under ±300 ppm.
4. Chaos: callbacks stall for 3 s (no error), burst 500 ms at once, send errors, and stop for good.
   Stalled and lost are detected, reopen happens, a late successful open is discarded, and a
   permanently stuck open leaves one helper and the `device_call_stuck` state, not more threads.
5. Isolation, on the real engine clock path: a pipeline with a synthetic video source plus
   simulated audio devices runs at 25, 29.97 and 60 fps while the devices stall, burst, die and
   are reopened 100 times. Assert: one frame per tick, PTS from the tick counter, no tick late
   because of audio, every queue at or below its bound, thread count and RSS flat.
6. Config, API and admission: validation cases; every control-plane mutation and import path
   rejects `audio_device`.

Hardware (macOS and Windows test hosts, not CI): open the default input and a USB interface, run
for 60 minutes, check unread fill, drops, servo ppm and the reported device rate. Play a tone to
the default output, check underruns, unplug and re-plug the USB device, change the OS default
device, and revoke and grant microphone permission; check that each recovers or reports the right
state.

## 7a. Dependencies and licences

- `cpal` and `rtrb` are pinned to exact versions in `Cargo.toml` (`=x.y.z`), optional, and pulled
  only by `audio-device`.
- CI adds a `cargo deny check --features audio-device` run for `aarch64-apple-darwin` and
  `x86_64-pc-windows-msvc` (the default graph check uses `all-features = false` and does not cover
  this feature), plus feature builds on the macOS and Windows legs. The full transitive licence
  result is recorded in the implementing PR. Linux and the default build pull none of it.

## 8. Not in scope

- Linux device audio (cpal ALSA/PipeWire): later.
- Exclusive-mode WASAPI and CoreAudio aggregate-device creation.
- Device audio as a clock master (never: invariant #1).
- Echo cancellation or monitoring mixes (switcher audio work).
