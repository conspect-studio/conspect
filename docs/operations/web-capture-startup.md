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

## Windy Premium account (issue #470)

Where an operator configures the Windy login: **Settings → Integrations → Windy** (email + password, then **Test login**). This is admin-only: it needs the `Admin` role and an unscoped key. Operators, viewers and object-scoped admins get `403`.

Provisioning is opt-in. Set `CONSPECT_WINDY_MASTER_KEY_FILE` to an absolute path of an owner-private (`0600`, no symlink) file holding exactly 32 random bytes, for example materialised from 1Password at boot. The encrypted store lives in `<CONSPECT_WEB_CAPTURE_ROOT>/windy-account/`, a `0700` directory created or adopted with the same checks as the other capture directories. Without the key file, `premium_available` stays `false` and the routes answer `409`.

| Route (admin) | Effect |
| --- | --- |
| `GET /api/v1/integrations/windy` | `{set, last_verified_at, result, premium_available, verifying}`. Never returns the email or password. |
| `PUT /api/v1/integrations/windy` `{email,password}` | Stores the credential encrypted and resets Premium. Write-only. |
| `DELETE /api/v1/integrations/windy` | Removes the credential and disables Premium. |
| `POST /api/v1/integrations/windy/verify` | Runs one bounded login check (about 10 s, hard cap 90 s; one at a time) and returns `result` = `premium`, `rejected` (wrong email or password), `not_premium` or `unavailable`. |

Decisions:

- **Storage.** The secret uses the same model as the managed Cloudflare credential: a `multiview-credentials` vault (`ChaCha20Poly1305`, AAD bound to the fixed owner `system:windy-account` and the reference) under an external master key. `windy-account.json` holds only the opaque `vault:` reference and the last verdict. The credential never goes into `conspect.toml`, `desired.json`, logs, support bundles, audit detail or API responses. Audit entries record only `set`/`verify` results. All account responses are `Cache-Control: no-store`.
- **Premium gate.** `capabilities.premium_available` is `true` only when the currently stored credential passed its latest check. Replacing the credential resets it. A verdict computed for a credential that was replaced mid-check is discarded. A capture with `view.public = false` fails closed with `409` unless Premium is available. It never falls back to the public page. A login check is revoke-first: the gate is cleared in memory and `unverified` is made durable before the browser runs, and a Premium result is published only after it is persisted. A failed check, an unresolvable credential or a failed write therefore never leaves Premium on, now or after a restart.
- **No cross-mode coalescing.** The capture service coalesces a new request onto a fill's active job only when the request is identical: a SHA-256 of the capture request with the credential removed, which keeps `require_premium`. Any other request for a busy fill (for example Premium while a public job runs) gets `409`.
- **Secret-free rejections.** The credential `PUT` parses its body by hand. Any malformed body gets one constant `422` (`no-store`), never serde diagnostics that could quote the input.
- **Resolver.** The credential is decrypted only for a Premium capture or a check. It goes to the browser host inside the existing stdin request JSON. It is never passed in argv or the environment; the environment is cleared.
- **Login per job (no persistent profile).** Each job uses its throwaway `0700` profile and logs in again, which adds about 5 s per job. A persistent logged-in profile would keep a long-lived Windy session cookie at rest outside the encrypted vault, and its lifetime would be shared across concurrent jobs. That was not judged simple or safe enough.
- **Expiry and re-login.** The host decides from each page sample (`premiumStep` in `template_windy.cjs`). If the login icon reappears (session expired or dropped), it submits the form again, up to two logins per job. A Windy "Login failed" message becomes `Windy login rejected`. Logged in without a Premium subscription becomes `Windy account has no Premium`. A Premium session lost during frame capture fails the job (`Windy Premium session lost`). Premium is detected from `localStorage.settings_subscription` containing `premium` and the login icon being gone.
- **UI.** The capture dialog's **Use Premium** is on by default when `premium_available`. Otherwise it sends `public: true`.

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
11. Windy Premium: a wrong password reports `rejected`, the test account reports `premium`, and the secret appears in no response, audit entry, log or plaintext file (`tests/windy_account.rs`).

No production installation or restart has been performed by this runbook.
