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

1. **One watchdog thread, a wait-free arm.**
   - `exit_watchdog::enable()` runs once in the `multiview` binary's `main`. It starts one
     dedicated OS thread that parks. Library users (tests, other binaries) never call it, so
     nothing can end their process.
   - `exit_watchdog::arm(cause)` is wait-free and allocation-free and never logs: it
     compare-and-sets a deadline, raises the cause, and unparks the thread. So it is safe to
     call from any thread, including the output clock.
   - The first arm fixes the deadline. A later `Fatal` upgrades a `Stop` (a run that fails
     during its stop reports failure); a `Stop` never downgrades a `Fatal`.
   - At the deadline, if the process is still alive, the thread writes one line to
     `/dev/stderr` through its own file handle (no std stderr lock, no tracing: either may be
     the stuck part) and calls `std::process::exit` with the cause's code: 1 for a fatal, 0 for
     a stop.
2. **Arm points: every exit decision.**
   - The bake consumer returns a fatal error (live run only, never with a test seam), or the
     consumer could not be spawned.
   - The output clock sees the egress fatal flag (`stop_on_egress_failure`) or a disconnected hot
     queue. This also covers a consumer that died without returning, for example a panic.
   - The engine loop returns an error.
   - A stop signal (SIGTERM or Ctrl-C).
   - Not armed: a failed non-run command, an offline render, the in-process test seams.
3. **The bound.** `EXIT_DEADLINE = 2 s` from the decision to the process exit.
   - The measured useful teardown takes about 0.2 s: egress (encoder flush, muxer trailer, HLS
     `#EXT-X-ENDLIST`) and the ProgramSet shutdown. So 2 s is a 10× margin.
   - The sink wedge grace (`SINK_WEDGE_GRACE`, 2 s) still applies inside it: a wedged sink is
     detached or cut. That is the same outcome as before, but sooner.
4. **Useful work first.**
   - The pipeline teardown now joins egress (consumer and sinks: trailers, HLS finalize)
     **before** the source teardown. So the outputs are final within about 100 ms of the clock
     returning, inside the bound.
   - Source teardown (bounded and warned per producer) runs after it, and the deadline cuts
     whatever is left.
   - On a stop, the signal task also starts the ADR-W024 final Running persist and the
     event-journal flush right away, concurrently with the pipeline teardown, so they land
     inside the bound. The ordinary end-of-run persist still runs if there is time (same state,
     idempotent).
   - The control plane's graceful drain (10 s ceiling) is cut by the deadline. Clients
     reconnect to the restarted process.

## Why the outputs stay valid at a hard exit

- **HLS manifests** are always written as `<name>.tmp`, fsync, rename
  ([hls-manifest-publication](hls-manifest-publication.md)), so at any instant the on-disk
  manifest is the last committed one. A hard exit before finalize omits `#EXT-X-ENDLIST`. That is
  the same state as a crash, and players handle it.
- **HLS segments** are muxed into `<seg>.tmp` and renamed after they close. A segment that was
  open at the exit stays a `.tmp` file, which no manifest lists.
- **PR 255** (HLS publish on the error path) lands its finalize inside the egress join, which now
  runs first. The chaos test requires `#EXT-X-ENDLIST`.
- **File outputs** (`program.ts`, `.mkv`, `.mp4` targets) write their final path in place. Their
  trailer is part of the egress join, which runs first and takes milliseconds. A file is only
  truncated if its sink is wedged past `SINK_WEDGE_GRACE`, which is the same as before. For MP4,
  a missing `moov` atom makes the file unplayable. That risk is unchanged and is not made more
  likely by this change.
- **Network outputs** (SRT, RTMP, WebRTC, MoQ): the receiver sees the end of a stream either way.
- **Temp files:** a hard exit can leave one manifest `.tmp` per playlist and one open segment
  `.tmp`. No manifest lists them, and they are bounded per exit. Startup clean-up of stale
  temps is a separate change.

## Restart rate

- Before this change, a persistent fault cycled every 15 to 47 s; it can now cycle every few
  seconds.
- A systemd deployment with `Restart=always`, `RestartSec=2s` and systemd's default start
  limit (5 starts in 10 s) that could stop the unit if the engine fails within about 2 s of every
  start.
- The faults measured on #309 came 3 to 150 min apart, so this is not a present risk. The
  deployment runbook must state the start-limit settings (`StartLimitIntervalSec=0`, or a burst
  that covers a persistent fault), and the operator should watch the restart counter.

## Invariant #1

The watchdog thread never touches the clock, and nothing on the clock path waits for it. The
change shortens dead output after a fault; it does not change the tick path.

## Tests

- **Unit:** the watchdog fires once at the deadline with the right code, does nothing when the
  process exits first, and keeps the first deadline when armed twice.
- **Chaos, mandatory** (`scripts/chaos-bounded-exit.sh`, real binary): a `_chaos-test-seams` build
  stalls the bake consumer once for 3 s (about 90 ticks, far past the 8-tick envelope, so the run
  must fail "fell N ticks behind"), and a teardown seam holds source teardown for 20 s. For every
  run it requires:
  - the fatal line, then exit code 1 within 2 s + 0.5 s of it;
  - the watchdog's armed line and its hard-exit line (the held teardown cannot finish in time on
    its own);
  - a manifest that parses, has `#EXT-X-ENDLIST` and lists only complete segments.

  The same for SIGTERM (sent once the pipeline runs and the process is alive), with exit code 0.
  Locally: 20 × fatal and 20 × stop. CI (macOS job): 10 × each.
- **Negative control:** with `enable()` removed, the same runs exit after 40 s and fail.
- **Release guard:** the `_chaos-test-seams` feature is in the `*test-seam*` family, so the
  release-feature guard keeps it out of every shipped preset.

## Not in scope

- Faster source teardown, for example cancellable libav reads. This is a separate improvement:
  the deadline makes it a speed-up, not a correctness need.
- Moving the output clock to its own thread (front-door lane #287).
- The cause of the falls themselves (PR 223, PR 237 and the WHEP lead on #309).
