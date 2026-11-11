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
| Output drain (normal priority) | the existing display-audio drain loop | publisher FIFO (short lock), resampler, servo, ring (push) | hold the FIFO lock across anything but an in-memory copy |
| Lifecycle helper | `multiview-audio-device` | cpal open/close, with a deadline | run on the engine or pump thread |
| Output clock / program bus | engine | `AudioStore::read` (wait-free); `DisplayAudioPublisher::push_block` (short copy) | wait for a device, a ring or a lifecycle event |

The rings are preallocated `rtrb` SPSC rings. Push and pop are wait-free on both sides.

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
- **Rate:** the stream is opened at 48 kHz, the canonical rate. `AdaptiveResampler` is a
  same-rate time stretch (±5000 ppm at most), not a rate converter. A device that does not list
  48 kHz in its supported configs is refused with a latched warning (CoreAudio and WASAPI shared
  mode offer 48 kHz on the device classes we target). A fixed-ratio stage for other rates is a
  later, separate change.
- **Pump:** wakes every 5 ms (bounded sleep, stop-flag aware). It pops everything, maps the
  channels to the source layout (`ChannelMatrix`), and time-stretches with one
  `AdaptiveResampler`. The trim is the servo output: it holds `AudioStore::buffered_frames` at the
  target (prebuffer) within ±500 ppm, so a device crystal that is fast or slow against the output
  clock is absorbed and the store neither drains nor overflows.
- **Store:** `AudioStore::live(48 kHz, capacity ≈ 1 s, prebuffer ≈ 60 ms)`. A read is gap-free:
  silence for anything not written. A stalled or lost device reads as silence, and the picture and
  the tick do not change.

## 3. Output path

```
program bus ──push_block──▶ [publisher FIFO, bounded drop-oldest] ──drain loop (servo + resampler + xrun)
                                                                         └──write──▶ [SPSC ring] ──pop──▶ device callback
```

- The engine uses the existing `DisplayAudioPublisher`. Its push is one short in-memory copy under
  a lock that the drain loop holds only for in-memory copies.
- The existing `drain_loop` runs unchanged over a new `AlsaSink` implementation (`CpalSink`):
  - `open(params)` asks the lifecycle helper to open a cpal output stream at `params.sample_rate`
    (always 48 kHz, see below) and `params.channels`, with a deadline (2 s). On timeout or error,
    `open` returns `Err` and the loop retries on its slow cadence.
  - `write` pushes as many frames as fit into the ring and returns `Wrote(n)` immediately.
    `n = 0` (ring full) is the normal back-off signal. The loop's bounded retry
    (`MAX_STALLED_WRITES`) then drops the remainder.
  - `delay_frames` returns the ring fill plus the device latency that cpal reports, for the skew
    tracker.
  - `recover` reopens the stream when the error callback has marked it lost.
- **Callback:** pop up to the requested frames, convert to the device sample format, fill any
  shortfall with silence and add it to `underrun_frames`.
- **Rates:** the drain loop and the servo correct only within ±ppm, so the output device must run
  at 48 kHz. The capability (`EldSource` implementation) reports 48 kHz only when cpal lists a
  supported config containing it. CoreAudio and WASAPI shared mode both offer 48 kHz on every
  device class we target. A device that cannot take 48 kHz stays silent with a latched warning
  (the same rule as an HDMI sink). A fixed-ratio stage for 44.1 kHz-only devices is a later,
  separate change.

## 4. Device lifecycle and errors

- Devices are found by exact cpal name, or `default`. Lookup and open run on the lifecycle helper
  with a deadline; there is at most one open in flight per device.
- The cpal error callback sets an atomic `lost` flag and records the error kind. It does nothing
  else.
- Input: the pump sees `lost`, drops the stream, and reopens with capped exponential backoff
  (250 ms → 10 s). The store reads silence meanwhile.
- Output: the drain loop sees `recover` → reopen, same backoff. The FIFO keeps dropping the oldest
  frames, so the engine push stays bounded.
- No callback for a set time (2 s while open) also counts as lost. This covers a denied microphone
  that "opens" but never calls back.
- Every warning is latched (logged once per state change), never per poll.

## 5. Permissions

- **macOS:** microphone use needs TCC consent for the top-level binary. A process without it gets
  silence (CoreAudio delivers zeros) or no callbacks. The source reports `permission_suspected`
  when it receives only exact zeros for 2 s after open, as a typed warning; it does not stop.
- **Windows:** the per-app microphone privacy setting. A denied open fails with an access error;
  the source reports `permission_denied` and retries on the slow cadence.

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
- The control API refuses to create or patch `audio_device` sources and outputs. Local hardware
  is file-configured only (the same rule as capture devices), so a remote API client cannot open a
  microphone on the host.

## 7. Tests (failing first)

CI, no hardware, over a **simulated device**: a thread that calls the same callback closure on its
own clock.

1. Ring adapters: push/pop bounds, overflow and underrun counts, silence fill.
2. Pump + servo: a device that is +300 ppm and −300 ppm against a simulated output clock for 10
   simulated minutes keeps the store fill within the target band (no unbounded growth or drain).
3. Chaos: the callback stalls for 3 s, bursts 500 ms at once, sends errors, then recovers. The
   reader gets silence then audio, all buffers stay at or below their bounds, and a reopen happens.
4. Isolation soak: a simulated engine tick loop reads the store and pushes the output FIFO at
   25 fps for 60 s while the simulated devices stall, burst and die. The longest engine-side
   call stays under 1 ms, and no tick is late because of a device.
5. Config and API: validation cases; the control API returns 4xx for `audio_device`.

Hardware (macOS and Windows test hosts, not CI): open the default input and a USB interface, run
for 10 minutes, check store fill, drops, and the reported device rate. Play a tone to the default
output, check underruns, unplug and re-plug the USB device, and check that it recovers.

## 8. Not in scope

- Linux device audio (cpal ALSA/PipeWire): later.
- Exclusive-mode WASAPI and CoreAudio aggregate-device creation.
- Device audio as a clock master (never: invariant #1).
- Echo cancellation or monitoring mixes (switcher audio work).
