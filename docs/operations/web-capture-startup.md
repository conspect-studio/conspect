# Web capture startup (candidate, not deployed)

This candidate starts a real background capture service only when explicitly configured. It does not load the result into a player automatically. Premium capture and URL downloads are unavailable until their separate capability gates pass.

## Provisioning

The service account must own a private capture directory beneath a service-controlled parent. Do not choose a user-writable or symlinked parent. Browser profiles, temporary media and credentials must never enter support bundles.

Deployment configuration (absolute executable paths; examples must be replaced with installed paths):

```ini
CONSPECT_WEB_CAPTURE=1
CONSPECT_WEB_CAPTURE_ROOT=/home/operations/mv/capture
CONSPECT_WEB_NODE=/usr/bin/node
CONSPECT_WEB_BROWSER=/opt/chromium/chrome-headless-shell
CONSPECT_WEB_FFMPEG=/opt/multiview-ffmpeg/bin/ffmpeg
```

Optional `CONSPECT_WEB_BROWSER_GL` selects the browser GL backend (deployment-only, never from HTTP; any other value aborts startup):

| Value | Flags | Use |
|---|---|---|
| unset / `default` | none | browser's own selection |
| `swiftshader` | `--use-angle=swiftshader --enable-unsafe-swiftshader` | GPU-less host; Windy radar is WebGL and renders empty without a GL backend |
| `egl` | `--use-gl=angle --use-angle=gl-egl` | host with a real GPU/EGL stack |

No mode adds `--disable-gpu`. A radar step capture without a working WebGL context fails with `radar layer did not render (GL unavailable?)` instead of committing a blank loop.

The FFmpeg build needs PNG decoding, image2pipe input, FFV1 encoding and MOV muxing. No GPL encoder is required. The browser sandbox must function; this integration does not insert `--no-sandbox`.

Configuring these values does not prove deployment. Build and test the actual binary, coordinate its restart with the host owner, then verify the service on the actual public management URL. Keep a rollback binary and the previous desired configuration.

## HTTP contract

`GET /api/v1/media/captures/capabilities` reports actual configured availability and unavailable reasons. Do not treat unavailable Premium or URL fetch as a successful feature.

`POST /api/v1/media/captures` accepts a bounded Windy view, fill identity, optional target-player identity and explicit acceptance of the current terms version. It never accepts HTML, JavaScript, browser executable paths, passwords or arbitrary URLs. Mutations require an authenticated write-capable actor and object authorization. Idempotency and durable acceptance are release gates in this candidate.

Capture mode (`view` fields, all optional): `capture` = `steps` (default for `overlay = "radar"`; radar only) iterates the radar timeline and writes one frame per distinct step, or `realtime` (default for wind) records `frames` wall-clock screenshots. Steps-mode knobs: `timing` = `equal` (default; every step `frame_ms`) or `proportional` (duration follows the real gap between radar timestamps, mean `frame_ms`, clamped to `[frame_ms/4, frame_ms*4]`); `frame_ms` 100–5000 (default 600); `end_hold_ms` 0–10000 (default 1000, held on the newest step); `max_steps` 2–48 (default 12 most recent steps). The clip stays constant-rate FFV1 MOV at `fps`: each step PNG is repeated `round(duration*fps/1000)` times (at least once, cumulative integer rounding), the total is capped at 600 frames, and in steps mode `frames` is ignored. Byte-identical consecutive steps are dropped; fewer than two distinct steps fails with `radar loop has fewer than 2 distinct frames`. Each step waits at most 15 s for the map to settle. The producer's `capture_complete` record reports `steps_found`, `steps_kept`, `steps_dropped_duplicate`, `frames`, `timing` and `hold_frames`.

Every capture and cancel POST requires an `Idempotency-Key`. The actor, request digest and acceptance receipt persist before acknowledgement. The same key and body replay the original job; a different body produces a conflict.

After an abrupt restart, a pending receipt can return HTTP 409 with problem type `/problems/capture-outcome-uncertain`. The original request may or may not have run. The same key never automatically reruns it. Do not bypass the uncertain receipt with a new key. An administrator must first stop the affected producer, verify its generation and any artifact, and record an explicit recovery decision. Durable request-to-job correlation now recovers an exact retained job ID without another producer. A historic receipt without a correlation, or a correlation outside the bounded service history, stays uncertain and requires manual recovery. This is an at-most-once admission guarantee, not a claim of exactly-once execution. Payloads, authored documents and credentials are not journaled.

Artifact transfer has a five-second send idle timeout and a thirty-second absolute deadline. The response carries the committed content length. A timeout is an incomplete transfer, never a valid shorter clip.

A `202` response means a job was accepted. Follow the returned `status_url`. Only `state = succeeded` plus an immutable `artifact_id` means media was committed. `failed`, `cancelled` and `uncertain` are not success. A job failure must retain the previous good artifact.

`GET /api/v1/media/captures/{id}/artifact` checks the owning actor and fill before pinning and streaming the immutable file. The stream uses bounded chunks and must release its lease if the client disconnects. It never resolves an API-supplied local file path.

A target-player name in a request does not mean playback has occurred. Library promotion and the player load acknowledgement must be verified separately. Do not label the workflow "VT ready" until that acknowledgement exists.

## Scheduled fill refresh (issue #472)

A control-plane tokio task (`multiview_control::fill_refresh`, spawned beside the HTTP server and aborted with it) re-captures every `refresh = { mode = "interval", minutes = N }` fill and reloads its player. It never runs on, and is never awaited by, the engine data plane. Each tick runs on the blocking pool, and the next tick waits for it, so ticks never overlap and nothing queues up.

* **Sources.** `[[media_fill]]` entries with `kind = "web_capture"`, `template = "windy"`, an interval refresh, `view.params.{lat,lon,zoom,overlay}` and current terms are scheduled at startup. The config view has no fps, so these captures use 10 fps / 60 frames and the default 12 steps. The output-cadence conform is a separate lane. The Web dialog can also register a runtime schedule with `PUT /api/v1/media/fills/{id}/refresh` `{minutes, then, capture, start_now}`, which stores the exact `CaptureBody` the dialog submitted. Runtime schedules are in-memory and are lost on restart; config schedules are the durable form. You cannot change or delete a config fill through the API (409).
* **Principal (no escalation).** A config fill runs as the synthetic principal `media-fill:<id>`: role operator, object scope exactly `[fill, player]`, empty output and discovery scope. An API schedule runs as the API key that registered it. That key is re-resolved from the live key store before every submit, poll and load, so revoking or re-scoping the key stops the schedule at its next step, and the run never loads. Capture jobs are owner-scoped, so a job started by one principal is visible only to that principal.
* **Timing.** Due times sit on a stable per-fill slot grid, `slot_start = k·interval + (sha256(fill) mod interval)`, which spreads fills across the interval. The tick runs every 30 s while idle and every 5 s while a run is in flight. A config fill is first due in the slot that contains startup. A dialog schedule is first due in the next slot, because the dialog's own capture supplies the first clip.
* **Single flight, bounded.** Each fill has at most one run in flight (`capturing` → `loading`). At most 1 scheduled run is in flight overall (`MAX_IN_FLIGHT`), and at most 32 fills are scheduled (`MAX_FILLS`, the same as the service's `max_fills`). Due fills start earliest-due first. The scheduler never floods the capture queue, whose capacity is 4.
* **Run.** The scheduler submits through the same `Provider::submit` path the HTTP route uses. The capture `Idempotency-Key` is `fill-refresh:<h(fill)>:<slot>:<h(body)>`, so a restart inside a slot replays the same job through the durable ledger instead of starting a second browser. A pending receipt from before a crash comes back as uncertain and counts as a failed run, never as a re-run. The capture deadline is 300 s. When the capture succeeds, the scheduler pins the artifact (`pin_for_player`) and calls `PlayerLoadProvider::load` with `then = resume` for config fills or the dialog's choice for API schedules, using the load key `fill-refresh-load:<h(fill)>:<job>`. The load deadline is 60 s. A run counts as successful only when the native load reports `completed`.
* **`during_refresh = play_last`.** The scheduler does not touch the player until a capture has succeeded, and the native load keeps the old clip on air until its ACK. `during_refresh = "na"` is not implemented and behaves like `play_last`.
* **Failure.** A failed run (submit refused, capture failed/cancelled/uncertain, deadline, load failed/superseded, authorization lost) leaves the old clip playing and records `last_error`, truncated to 256 characters. The fill is retried after `min(interval, 30 s · 2^(failures−1))`. A success clears the error and the failure count.
* **Status.** `GET /api/v1/media/fills/refresh` (list) and `GET /api/v1/media/fills/{id}/refresh` return `{fill_id, player, source, minutes, during_refresh, then, phase, in_flight, next_due, last_success_at, last_artifact_id, last_capture_job_id, last_error, consecutive_failures}`. Reads need the read role plus object authorization on both the fill and its player. A row you cannot see is filtered from the list and returns 404 on `GET`. `DELETE /api/v1/media/fills/{id}/refresh` stops an API schedule, and the current clip keeps playing. The Windy dialog and every media-player card show this status.

## Required verification before deployment

1. Cold/default GPU-free control build and targeted capture tests.
2. Role and per-object denials before any browser allocation.
3. Same-role foreign actor cannot inspect, cancel or download a job.
4. Actual IPv6 HTTP POST reaches the real service and returns a committed artifact.
5. Actual browser-generated media is decodable, has the expected frames and correct colour tags, and contains no unwanted page controls.
6. Cancellation and SIGTERM reap the producer without publishing partial content.
7. Idempotent retries do not start a second browser after the first job completes.
8. Restart recovers terms/job state without reusing identities or inventing success.
9. Missing network isolation keeps URL fetch unavailable.
10. Generated OpenAPI and client schema match the mounted routes.

No production installation or restart has been performed by this runbook.
