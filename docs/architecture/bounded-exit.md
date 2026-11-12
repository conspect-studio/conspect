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
   - The first arm fixes the deadline. A later `Fatal` upgrades a `Stop`: a run that fails
     during its stop reports failure, and the deadline moves to the fatal's own bound if that is
     later. A `Stop` never downgrades a `Fatal`.
   - At the deadline, if the process is still alive, the thread acts as follows:
     - It writes one line to `/dev/stderr` from a helper thread, through its own file handle
       (no std stderr lock, no tracing: either may be the stuck part).
     - It calls `std::process::exit` with the cause's code: 1 for a fatal, 0 for a stop.
       A stop exits 1 instead when outputs are still open, or when the run's own verdict
       is a failure (it faltered: frames were shed). That is the same code a normal return
       would give.
     - A backstop thread ends the process if the C runtime exit path (atexit handlers, stdio
       flush) has not finished within 500 ms: `SIGKILL` in builds with `rustix` (every
       `ffmpeg` build), `abort()` otherwise. Both skip atexit handlers.
2. **Arm points: every exit decision.**
   - The bake consumer returns a fatal error (live run only, never with a test seam), or the
     consumer could not be spawned.
   - The output clock sees the egress fatal flag (`stop_on_egress_failure`) or a disconnected hot
     queue. This also covers a consumer that died without returning, for example a panic.
   - The engine loop returns an error.
   - The egress join returns an error. This upgrades a stop to a fatal, and the fatal gets its
     own full bound. On a consumer error, the join still waits for the sinks (bounded) before it
     returns, so their trailers and HLS finalize land.
   - **Outputs still open at the deadline.** Each live egress counts as open until its join has
     resolved every output, on every return path (a drop guard). An output stays open only when
     the consumer is still running at the grace, or a sink is detached as wedged (an explicit
     flag). If a stop reaches the deadline with outputs still open, it exits 1, not 0: the
     outputs did not finalise. This covers a wedged consumer and a wedged sink alike. A slow
     but healthy drain that finishes before the deadline still exits 0. There is no progress
     heuristic: only the result matters.
   - Any failed `run` (startup or engine, full pipeline or `--software`).
   - A stop signal (SIGTERM or Ctrl-C).
   - Not armed: a failed non-run command, an offline render (every arm in the pipeline is gated
     on the live drop-on-overload policy), the in-process test seams.
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
   - As soon as any exit is decided (a stop or a fatal), a dedicated OS thread with its own
     small runtime starts the durable shutdown work, concurrently with the pipeline teardown:
     - the control plane's graceful drain;
     - the ADR-W024 final Running persist;
     - the event-journal flush (first, and it waits up to 500 ms for the worker to apply it).

     It does not wait for the main runtime, whose workers may be busy with the synchronous
     teardown. The ordinary end-of-run steps still run if there is time (the persist is
     idempotent).
   - The drain (10 s ceiling) is cut by the deadline. In-flight requests get the remainder of
     the bound, about 2 s. A client whose request was cut cannot tell whether a mutation landed.
     The Running persist that runs at the decision captures any mutation committed before it.

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
  trailer is part of the egress join, which runs first and takes milliseconds.
  - **New truncation band:** a finalise that takes longer than the bound (about 2 s) is now cut.
    Before this change, it got the 13–44 s teardown window and usually completed.
  - The egress join's sink budget (`SINK_WEDGE_GRACE`, 2 s) starts at the same instant as the
    exit bound. A sink that is still finalising at the bound is cut, and its `WEDGED` report
    line is not printed.
  - For MP4, a missing `moov` atom makes the file unplayable.
  - The MPEG-TS outputs the engine ships for live use (HLS segments, `program.ts`) stay
    readable up to the cut.
  - Recorded file outputs that need a trailer, and the cost of a longer bound for them, are a
    follow-up.
- **Network outputs** (SRT, RTMP, WebRTC, MoQ): the receiver sees the end of a stream either way.
- **Temp files:** a hard exit can leave one manifest `.tmp` per playlist and one open segment
  `.tmp`. No manifest lists them, and they are bounded per exit. Startup clean-up of stale
  temps is a separate change.

## Restart rate

- Before this change, a persistent fault cycled every 15 to 47 s; it can now cycle every few
  seconds.
- With `Restart=always`, `RestartSec=2s` and systemd's default start limit (5 starts in 10 s),
  systemd could stop the unit if the engine fails within about 2 s of every start.
- The faults measured on #309 came 3 to 150 min apart, so this is not a present risk.
- The required supervisor settings (`StartLimitIntervalSec=0`, `Restart=always`) and the restart
  counter to watch are in
  [building §4.1](../operations/building.md#41-running-under-a-supervisor-exit-and-restart).

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
  - a manifest that parses, has `#EXT-X-ENDLIST` and lists only complete segments;
  - `stage="egress-joined"` logged before the held source teardown begins;
  - the exit-time Running persist written (`active.toml`; the test config runs the control
    plane).

  The same for SIGTERM (sent once the pipeline runs and the process is alive), with exit code 0.
  The `stopwedge` mode sends SIGTERM while the consumer is stalled (an output that cannot
  finalise during a stop). It requires exit code 1 within 2 s + 0.5 s of the signal.

  The `falterstop` mode stalls the consumer for 200 ms (frames are shed but the envelope holds),
  then sends SIGTERM. It requires exit code 1, because the run faltered.

  On the Apple-silicon host: 20 × each of the four modes, 0 failures, exits at 2.08–2.13 s. CI
  (macOS job) runs 10 × each mode.
- The script also proves that the watchdog ended the process:
  - no normal-return stage (`run-tenants`) is logged;
  - the exit code equals the code in the watchdog's line;
  - every wait is bounded, and a run that does not exit is killed and fails.
- **Negative control:** with `enable()` removed, the same runs exit after 40 s and fail.
- **Release guard:** the `_chaos-test-seams` feature is in the `*test-seam*` family, so the
  release-feature guard keeps it out of every shipped preset.

## Not in scope

- Faster source teardown, for example cancellable libav reads. This is a separate improvement:
  the deadline makes it a speed-up, not a correctness need.
- Moving the output clock to its own thread (front-door lane #287).
- The cause of the falls themselves (PR 223, PR 237 and the WHEP lead on #309).
