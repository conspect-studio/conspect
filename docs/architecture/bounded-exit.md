# Bounded exit after a fatal output fault or a stop signal (issue #309)

Status: design for the fix in issue #309. Class R3 (invariant #1).

## Problem

When the output falls behind the clock beyond the admitted envelope, the engine decides that it
cannot continue ("output fell N ticks behind the clock"). Its recovery is to exit and let systemd
start a fresh process. On 11 November the time from that decision to the process exit was 13 to
44 s. A clean `systemctl stop` took 17 s. For all of that time the outputs are dead and nothing is
restarting.

## Cause, measured (see the comment on #309 for the full table)

- **Egress is fast.** On a forced fatal, the encoder, muxer and HLS finalize stage joins within
  100 ms of the clock's return. The whole pipeline teardown, up to `main` returning, takes
  0.19 s on a small pipeline.
- **Source teardown is slow, and serial.** For each live source, the teardown waits up to
  `TEARDOWN_JOIN_GRACE` (3 s) to join the producer and up to 3 s again for its exit latch. The
  ingest supervisor waits up to `INGEST_JOIN_GRACE` (2 s) per thread. Producers blocked in a libav
  network read (HLS fetches, captions readers) never see the stop flag, so they use their full
  grace. In production this took 12 to 25 s.
- **The runtime drop after `main` is slow.** `#[tokio::main]` drops the runtime after `main`
  returns, and that waits for blocking-pool work that is still running. In production this took
  0.7 to 17 s.

## Decision

1. **An exit deadline, armed once.** A process-wide `ExitWatchdog` is armed at the first of:
   - the fatal output decision, where the bake consumer records its error;
   - a stop signal (SIGTERM or Ctrl-C);
   - the pipeline run returning an error.

   Arming starts one dedicated OS thread (not a Tokio task, so a blocked runtime cannot delay
   it). The thread sleeps until the deadline. If the process is still alive at the deadline, it
   logs one line that names the cause, flushes stderr, and calls `std::process::exit` with the
   code the run would have returned: 1 for a fatal, 0 for a clean stop. Arming is idempotent: the
   first deadline wins.

2. **The bound.** `EXIT_DEADLINE = 2 s` from the decision to the process exit.
   - The measured useful teardown (egress, encoder trailer, HLS finalize, ProgramSet shutdown)
     takes 0.2 s, so 2 s leaves a 10× margin.
   - systemd restarts the service about 2 s after the exit, so the worst case is about 4 s of dead
     output instead of 15–47 s.
   - The deadline is a constant with a test, not a config knob. It is not shortened per stage.

3. **Useful work first, then the slow work, which the deadline cuts off.**
   - The run already joins egress (encoder flush, muxer trailer, HLS `#EXT-X-ENDLIST`) inside the
     pipeline teardown. After a fatal, that join moves before source teardown, so the important
     part is done in the first ~100 ms.
   - Source teardown stays as it is: bounded, warned, detach-on-overrun. It is not made faster
     here. The deadline ends whatever is left.
   - The event journal flush (`event_journal::flush`) runs before the watchdog fires. It only
     posts to an in-memory feed, and the loss of a counter still queued at a hard exit is
     already documented.

4. **The control plane.** It gets no extra time. Its graceful drain (10 s ceiling) is cut by the
   deadline. Clients reconnect to the restarted process.

## Why the outputs stay valid at a hard exit

- **HLS manifests** are always written as `<name>.tmp`, fsync, rename
  ([hls-manifest-publication](hls-manifest-publication.md)), so at any instant the on-disk
  manifest is the last committed one. A hard exit before finalize omits `#EXT-X-ENDLIST`. That is
  the same state as a crash, and players handle it.
- **HLS segments** are muxed into `<seg>.tmp` and renamed after they close. A segment that was
  open at the exit stays a `.tmp` file, which no manifest lists.
- **PR 255** (HLS publish on the error path) lands its finalize inside the egress join. Moving the
  egress join earlier keeps that, and the chaos test checks it.
- **Other outputs** (SRT, RTMP, WebRTC, MoQ) are network streams. The receiver sees the end of a
  stream either way.

## Invariant #1

The watchdog thread never touches the clock, and nothing on the clock path waits for it. The
change shortens dead output after a fault; it does not change the tick path.

## Tests

- **Unit:** the watchdog fires once at the deadline with the right code, does nothing when the
  process exits first, and keeps the first deadline when armed twice.
- **Chaos, mandatory:** a `_chaos-test-seams` build stalls the bake consumer once (the output falls
  behind the clock), and a slow-teardown seam keeps a producer blocked past every grace. The
  test runs the real binary and asserts:
  - the process exits with code 1 within 2 s + 0.5 s of the fatal log line;
  - the HLS manifest on disk parses and lists only complete segments;
  - this holds over 20 consecutive runs.
- **SIGTERM:** the same with a stop signal: exit code 0 within the bound.
- **Release guard:** the `_chaos-test-seams` feature is in the `*test-seam*` family, so the
  release-feature guard keeps it out of every shipped preset.

## Not in scope

- Faster source teardown, for example cancellable libav reads. This is a separate improvement:
  the deadline makes it a speed-up, not a correctness need.
- Moving the output clock to its own thread (front-door lane #287).
- The cause of the falls themselves (PR 223, PR 237 and the WHEP lead on #309).
