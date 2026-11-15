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

### Values deployed on mda001

These are the values the live host runs with (issue #474). Each path is a real executable, not a wrapper or symlink; startup refuses symlinks.

```ini
CONSPECT_WEB_BROWSER=/usr/lib/chromium/chromium-headless-shell
CONSPECT_WEB_FFMPEG=/usr/bin/ffmpeg
CONSPECT_WEB_BROWSER_GL=egl
```

- Browser: the Debian `chromium-headless-shell` package binary under `/usr/lib/chromium/`. `/usr/bin/chromium*` are `sh` wrapper scripts and are not usable as the producer browser.
- FFmpeg: the distribution `/usr/bin/ffmpeg` 7.1.5. The pinned `/opt/multiview-ffmpeg` build lacks the PNG decoder and the `image2pipe` demuxer the producer needs, so it cannot encode a capture.
- GL: `egl` (ANGLE `gl-egl`) on the NVIDIA GPU. The host has no `/dev/dri` render node, so the browser default cannot reach the GPU; without `egl` Windy radar renders empty.

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

### Canvas cadence conform

A media player accepts only a clip whose frame rate equals the canvas rate exactly (a `30000/1001` canvas refuses a `25/1` clip with `wrong_cadence`). The control plane therefore conforms every capture to the running canvas cadence: the request's `fps` still paces the screenshots (`-framerate fps` on the PNG input), and the encoder adds `fps=num/den` to the filter chain and `-r num/den` on the output, so the MOV is constant frame rate with `r_frame_rate = avg_frame_rate = num/den` (exact rational, never a float). The frame count becomes `floor(written * num / (fps * den) + 1/2)` — the clip duration rounded to the nearest canvas frame, exactly as ffmpeg's `fps` filter (default `round=near`) emits it. The producer validates the decoded count against that number and reports it as `output_frames` in `capture_complete`; any mismatch fails the job. A `target_player` capture on a run whose canvas cadence is unknown is refused with `409` rather than producing an unloadable artifact.

### Load failure reasons

`GET /api/v1/media/players/{id}/load-operations/{operation}` reports, for `state = failed`, a typed `reason` and a short `detail`:

| `reason` | Meaning |
|---|---|
| `wrong_cadence` | clip frame rate ≠ canvas frame rate (detail names both, e.g. `clip frame rate 25/1 does not match the canvas frame rate 30000/1001`) |
| `vfr`, `no_cadence` | variable or missing frame rate |
| `alpha` | clip carries an alpha plane |
| `not_found`, `invalid_data`, `no_video_stream`, `decoder_open`, `no_frames` | the artifact could not be opened, parsed or decoded |
| `timeout` | open/prime deadline expired |
| `unknown_length`, `invalid_window` | playout window could not be resolved or exceeds a bound |
| `audio_unavailable` | the clip has audio but the player's audio rail is closed |
| `journal_failed`, `reference_missing` | the durable player binding refused the load |
| `other` | anything else |

`detail` is redacted: libav-derived failures carry a fixed message, never a filesystem path. The engine logs one `WARN media player load failed` line per failed operation (player, operation, reason) when the operation is first read as terminal; the output clock never logs it. The Windy fill dialog shows the reason next to the failed load.

### Putting a VT player on the desk

A declared `[[media_players]]` channel is only addressable by transport verbs. To route it to cells, switcher buses or routing crosspoints, declare a source that binds it:

```toml
[[sources]]
id = "vt-a"
kind = "media_player"
player = "vt-1"
```

The source opens nothing: its id resolves to the player channel's frame store (the player boot owns the decode thread), and its audio stays on the program bus once, under the player id; switcher audio-follow resolves `vt-a` to that channel. The source id must differ from every other player's id; reusing the bound player's own id (`id = "vt-1"`, `player = "vt-1"`) is allowed and names the channel itself. If the player could not boot (for example its journal is owned by another engine) the source reads no-signal and the engine still starts. Adding, editing or removing such a source on a running engine applies live (it is a binding change, not a producer start).

A target-player name in a request does not mean playback has occurred. Library promotion and the player load acknowledgement must be verified separately. Do not label the workflow "VT ready" until that acknowledgement exists.

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
