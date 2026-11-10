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

The FFmpeg build needs PNG decoding, image2pipe input, FFV1 encoding and MOV muxing. No GPL encoder is required. The browser sandbox must function; this integration does not insert `--no-sandbox`.

Configuring these values does not prove deployment. Build and test the actual binary, coordinate its restart with the host owner, then verify the service on the actual public management URL. Keep a rollback binary and the previous desired configuration.

## HTTP contract

`GET /api/v1/media/captures/capabilities` reports actual configured availability and unavailable reasons. Do not treat unavailable Premium or URL fetch as a successful feature.

`POST /api/v1/media/captures` accepts a bounded Windy view, fill identity, optional target-player identity and explicit acceptance of the current terms version. It never accepts HTML, JavaScript, browser executable paths, passwords or arbitrary URLs. Mutations require an authenticated write-capable actor and object authorization. Idempotency and durable acceptance are release gates in this candidate.

Every capture and cancel POST requires an `Idempotency-Key`. The actor, request digest and acceptance receipt persist before acknowledgement. The same key and body replay the original job; a different body produces a conflict.

After an abrupt restart, a pending receipt can return HTTP 409 with problem type `/problems/capture-outcome-uncertain`. The original request may or may not have run. The same key never automatically reruns it. Inspect the fill and its latest good artifact first. Use a new key only for an explicit rerun. This is an at-most-once admission guarantee, not a claim of exactly-once execution. Payloads, authored documents and credentials are not journaled.

Artifact transfer has a five-second send idle timeout and a thirty-second absolute deadline. The response carries the committed content length. A timeout is an incomplete transfer, never a valid shorter clip.

A `202` response means a job was accepted. Follow the returned `status_url`. Only `state = succeeded` plus an immutable `artifact_id` means media was committed. `failed`, `cancelled` and `uncertain` are not success. A job failure must retain the previous good artifact.

`GET /api/v1/media/captures/{id}/artifact` checks the owning actor and fill before pinning and streaming the immutable file. The stream uses bounded chunks and must release its lease if the client disconnects. It never resolves an API-supplied local file path.

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
