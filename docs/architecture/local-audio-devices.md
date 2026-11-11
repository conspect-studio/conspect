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
- **Heartbeat:** the input callback increments a monotonic callback counter (atomic). The pump
  declares the stream *stalled* when the counter has not moved for 2 s while open, and the
  lifecycle helper reopens it (section 4).
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
    store neither drains nor overflows.
- **Stale audio is never published (re-anchor).** The store's drop-oldest bound (1 s) is far
  above the target, so it does not shed a post-stall burst, and a ±500 ppm servo would take
  minutes. The pump therefore re-anchors itself:
  - If its own wake-up gap exceeds 50 ms, or the ring's overflow counter moved, everything in the
    ring is stale: the pump pops and discards it, resets the servo and resampler phase, and
    re-primes from fresh callback frames.
  - If `unread_frames` exceeds target + 100 ms (for example, the program bus paused reading), the
    pump discards incoming frames until unread is back at the target, then resets the servo.
  - Already-published audio is handled by the consumer: the store gets a `max_lag` of target +
    15 ms for this source kind. On `read`, if `head − cursor` exceeds it (the program bus paused or
    stopped reading, even briefly), the reader itself skips its cursor to `head − target` before
    reading. (The servo keeps normal lag within ±10 ms of the target, below this bound.) This
    is the existing consumer-owned re-sync logic in `AudioStore::read`, extended with one bound; it
    stays wait-free and only the consumer moves the cursor.
  - Discarded frames are counted (`stale_discard_frames`, `consumer_skip_frames`).
  - Test: after a pump stall of 50 ms to 5 s, and after a consumer pause of 50 ms to 5 s, the age
    of the first sample read is under target + 20 ms, and the budget below holds again within
    100 ms.
- **Latency budget:** device buffer (≈10 ms) + pump wake-up (≤5 ms) + servo target (80 ms) ≈
  95 ms constant from microphone to program bus, with ±10 ms variance. It is compensated with the
  existing per-source audio offset trim where the source is paired with video; a dedicated
  automatic lip-sync is not in scope.
- **Store:** `AudioStore::live(48 kHz, capacity ≈ 1 s, prebuffer ≈ 60 ms)`. The pump publishes at
  most every 10 ms (two pump wakes per publish); each publish copies the retained window once
  (copy-on-write). The simulated-device tests measure the pump's CPU time and allocation rate at
  this cadence. A read is gap-free:
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
- **The engine→drain ring is long-lived:** one per output, created at output start and never
  replaced. The device callback never touches it, so stream generations do not affect it, and the
  engine's push target never changes. During close, backoff and reopen, the drain keeps popping it
  and discards what it pops (the device is not playing), so the engine push stays bounded and drops
  nothing on its side. Only the device ring is per generation (section 4).
- **Drain** (a new loop in `multiview-audio-device`; it reuses `BufferServo`, `AdaptiveResampler`
  and `ChannelMatrix`, not the HDMI `drain_loop`, which depends on a PCM that blocks to pace it):
  - It is **device-paced**: it wakes every 5 ms (bounded sleep, stop-flag aware, so stall detection
    and stop are always reachable), and tops the device ring up to its target only when the fill
    is below it. The callback pop rate is the pace, so the drain cannot over-produce, whatever its
    own wake-up jitter.
  - **Stale backlog is never played (re-anchor).** The engine push drops only as an isolation
    safety valve (ring full), and what it drops is the newest audio. So on any engine drop, or a
    drain wake-up gap over 50 ms, everything in the engine→drain ring and the pending partial output
    is stale: the drain discards all of it, resets the servo and resampler phase, and re-primes from
    the next fresh engine frames. When the fill exceeds target + 100 ms without an engine drop (a
    slow drift or burst), the drain discards the oldest frames down to the target. Discards are
    counted (`stale_discard_frames`).
  - Test: after a drain stall of 50 ms to 5 s, the first frame played after recovery is from the
    current program time (content check with a tick-stamped test signal), not from before the stall.
  - The servo holds the engine→drain ring's fill near its target of 60 ms (ring capacity 200 ms),
    so the engine clock and the device clock are matched within ±500 ppm (the sign and reset rules
    are those of section 2).
  - Partial pushes are kept for the next round. Nothing is dropped while the device consumes.
- **Callback:** pop up to the requested frames in `CHUNK`-sized pieces, convert to the device
  format, and fill any shortfall with silence. It counts `underrun_frames` and stores a monotonic
  heartbeat (callback count) in an atomic.
- **Freshness in the callback.** The device ring's read side belongs to the callback, so the
  callback itself rejects old audio, without waiting for the drain:
  - The drain writes each chunk into the device ring with a header: the monotonic time it was
    written (an integer from `Instant`, taken on the drain thread) and the current `flush_epoch`.
  - The callback reads the monotonic clock once per call (a vDSO / `mach_absolute_time` read, no
    syscall or lock) and skips every chunk older than the device-ring target + 30 ms, or from an
    older `flush_epoch`, before it plays. Skipped frames are counted.
  - The drain increments `flush_epoch` on every re-anchor (engine drop, drain stall, heartbeat gap
    of 50 ms), so a drain-side re-anchor also invalidates what is already in the device ring.
  - Test: both interleavings (callback resumes before the drain notices; drain stalls while the
    callback runs) with a tick-stamped signal; the first frame played after each stall of 50 ms to
    2 s is from current program time within target + 30 ms.
- **Distinct states,** each with its own transition:
  - *underrun* (callback found the ring short): counted; the servo re-anchors; no reopen.
  - *ring full* (device not consuming but heartbeat advancing): normal, the drain waits.
  - *stalled* (heartbeat has not advanced for 2 s while open) or *lost* (error callback): the drain
    leaves the steady loop and asks the lifecycle helper to close the stream. Rings are reset or
    reused only after the close is confirmed (section 4, quiescence). Then servo reset and reopen
    with backoff.
- **Rates and channels:** the stream opens at 48 kHz. Channel negotiation is device-specific, not
  the HDMI ELD rule: the drain uses the device's own channel count from its supported configs
  (mono, stereo or multichannel) and folds or upmixes with `ChannelMatrix`. A device without a
  48 kHz config is refused with a latched warning.

## 4. Device lifecycle and errors

- **Discovery and identity.** `multiview devices audio` lists the host's input and output devices
  with cpal name, stable id, host API, the default flag, channel counts and supported rates. The
  stable id is cpal's `DeviceId` (`DeviceTrait::id`, a safe API): the CoreAudio device UID on macOS,
  the WASAPI endpoint id on Windows. cpal documents it as stable where possible; for these two backends it survives reboots and
  re-plugs in practice.
  - Config binds by `device_id` (preferred) or by exact `device` name. A name that matches more
    than one device is refused at open with a latched error that lists the matching ids. The name
    form never picks one by enumeration order.
  - `device = "default"` is the OS default and follows it, and the change is logged:
    - Windows (input and output) and macOS output: cpal reports the default change. On macOS
      output cpal reroutes the running stream itself; that `DeviceChanged` event is handled as a
      soft event (keep the stream, log, reset the servo), not as *lost*.
    - macOS input: cpal 0.18 has no default-input monitor. The pump compares
      `default_input_device()`'s id with the bound id every 2 s (on the pump thread, outside the
      callback) and reopens on a change.
  - A physical device literally named "default" is selected by its `device_id`.
  - Every open logs the name and id that were bound.
- **One lifecycle helper per device**, a long-lived thread that owns the cpal `Stream`. Open and
  close requests go to it through a 1-slot channel (single flight). The requester waits at most
  2 s for the reply, then treats the attempt as failed and moves to backoff.
- **Quiescence.** One stream and one callback own a ring at a time. After a stall or loss, a ring
  is reset or given to a new stream only when the helper confirms that the old stream was
  dropped (cpal stops the callback when the `Stream` is dropped). If the close does not complete
  within its deadline, the old generation keeps its ring and stream, no replacement stream is
  started, and the device goes to `device_call_stuck` until the close returns. Each new stream
  gets its rings fresh from a per-device pool of two, so a confirmed-closed generation's rings are
  reused only after confirmation.
- **Late results.** A reply that arrives after its deadline carries the request's generation
  number. The helper drops any stream whose generation is not current, so a late successful open
  is closed, not used.
- **A stuck helper.** If the helper is still inside a cpal call when the next request is due, no
  new helper is started (the thread budget is one per device). The device goes to the latched
  `device_call_stuck` state, its source reads silence (input) or its output drops (output), and a
  warning is raised. When the call returns, the helper resumes and normal backoff continues.
  At shutdown all helpers are told to stop at once and are awaited in parallel against one 2 s
  deadline; any still stuck is detached and the process exit takes it.
- The cpal error callback only stores the error kind in an atomic. The pump or drain maps it:
  - `DeviceChanged` (macOS default-output reroute by cpal): soft event; keep the stream, log,
    reset the servo.
  - `Xrun`: non-fatal; count it and re-anchor the servo; no reopen.
  - `PermissionDenied`: the latched `permission_denied` state; close, retry on the slow cadence
    (section 5).
  - `RealtimeDenied`: log once; the stream keeps running at normal priority.
  - `InvalidInput`, `UnsupportedConfig`, `UnsupportedOperation`, `Other` (cpal documents `Other`
    as permanent): the latched `config_error` state with the message; no automatic reopen. A config
    reload or a device change event (hot-plug, default change) clears it and allows one new
    attempt.
  - `DeviceNotAvailable`, `StreamInvalidated`, `DeviceBusy`, `ResourceExhausted`,
    `HostUnavailable`, `BackendError`: *lost*; close and reopen with backoff.
  - The same mapping applies to errors returned by device lookup and stream build/play, not only
    to the stream's error callback. Stalled/lost detection is in sections 2 and 3. Reopen uses capped exponential backoff
  (250 ms → 10 s).
- Every warning is latched (logged once per state change), never per poll.

## 5. Permissions

- **macOS:** microphone use needs TCC consent for the responsible (top-level) binary. The shipped
  app bundle and launcher carry `NSMicrophoneUsageDescription` and a stable signing identity, so
  the grant survives updates. Without consent CoreAudio may deliver zeros or no callbacks.
  - *No callbacks* is detected as *stalled* (section 4) and reported as
    `stalled (permission or device)`.
  - *Exact zeros* are reported as `input_silent` only.
  - **Known limit:** the leaf crate is `#![forbid(unsafe_code)]`, and cpal does not expose the TCC
    authorization status, so on macOS the source cannot tell a denied microphone from a silent
    one. It never reports `permission_denied` on macOS; the `input_silent` and `stalled` messages
    name microphone permission as one possible cause. Reading `AVCaptureDevice.authorizationStatus`
    would need an Objective-C call in a separate FFI crate; that is a later change if operators
    need it.
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
device_id = "CoreAudio:AppleUSBAudioEngine:...:1"   # stable id from `multiview devices audio` (preferred)
# device = "USB Advanced Audio Device"            # or an exact, unique name, or "default"
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
- **Read path:** the control API lists `audio_device` sources and outputs and their state to an
  authenticated client with read access to that resource (per-object authorization applies, as for
  every resource). Device ids and names are shown only to clients with the operator role; other
  readers see the source id and state only.
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
  result is recorded in the implementing PR.
- `cargo deny` checks licence compatibility only. The implementing PR also checks every new
  transitive crate for a NOTICE file and adds the required attribution to `THIRD-PARTY-NOTICES.md`
  (Apache-2.0 §4(d)). (Legal files such as `NOTICE` and `LICENSE*` are not edited as engineering
  work.)
- The cpal dependency is a target-specific dependency
  (`[target.'cfg(any(target_os = "macos", target_os = "windows"))'.dependencies]`). On Linux the
  `audio-device` feature compiles to a stub that refuses `audio_device` with a clear error, and
  pulls no libasound. Linux and the default build pull none of it.

## 8. Not in scope

- Linux device audio (cpal ALSA/PipeWire): later.
- Exclusive-mode WASAPI and CoreAudio aggregate-device creation.
- Device audio as a clock master (never: invariant #1).
- Echo cancellation or monitoring mixes (switcher audio work).
