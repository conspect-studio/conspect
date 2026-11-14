# ADR-0111: Google Cast as a patch-panel output: a `cast` output kind with its own CMAF/HLS packaging, a custom Web Receiver served by the engine, scoped stream tokens, and playback proof in CI

- **Status:** Accepted
- **Area:** Streaming / Output / Management / Web/API (invariants #1, #7, #10)
- **Date:** 2026-11-13
- **Source:** operator report of 13 Nov 2026 (quoted under Context); PR 362 (superseded)
- **Design note:** [cast-output.md](../architecture/cast-output.md)
- **Amends:** [ADR-M011](ADR-M011.md) (the media a Cast session plays, the receiver app, the LOAD fields; the session actor and discovery stay), [ADR-0110](ADR-0110-patch-panel.md) (adds the output kind `cast` to the capability table: one video slot, one audio slot)

## Context

The operator, 13 Nov 2026 (verbatim):

> you can't just throw HLS at a chromecast and have it work. And.. just for the avoidance of doubt - hls cast as it is now - does not work

> So, we will select a cast device from the multicast adverts - and start casting. We will select a layout or source to display?

The lead's binding answer: a cast is an output in the patch panel; changing what it shows is a take, with no re-cast.

What was true on `main` 93a35711:

1. ADR-M011 and PR 362 hand a Cast device the URL of an existing HLS output and Google's Default Media Receiver.
2. That output is built for desktop players: MPEG-TS segments of one GOP (1.001 s at 29.97 fps), a 6-segment window, video and each audio rendition in separate TS playlists, no `CODECS` for H.264 in the master.
3. The Cast Web Receiver SDK (3.0.0157) plays HLS with Shaka Player 4.15.56 and sets `streaming.rebufferingGoal = 10` s. Shaka holds the live edge three target durations back. Measured in headless Chrome with that configuration: the existing stream has a 3 s seek range and the player loops `seeking -> waiting` for about 13 s before it plays; Shaka never uses native MPEG-TS and transmuxes TS in JavaScript on the device.
4. Nothing reported what the receiver did: no receiver-side log, no state back to the operator, no server-side record of whether the device ever fetched anything.
5. The feature was never played by any automated test and never on a device.

What this ADR cannot claim: the harness reproduces the long start loop, not "never plays". The cause on the operator's device is unknown until the receiver log or the server's fetch record is read on that device. The certificate chain of the test host (Let's Encrypt's short-lived ECDSA hierarchy) is an independent candidate that no stream format fixes; the fetch record added here separates the two.

## Decision

1. **A `cast` output kind.** `[[outputs]] kind = "cast" id = "cast-1"` declares a Cast destination. It is an ordinary output of the patch (ADR-0108/0110): its feed is Program at start, a take moves it to Preview, Clean, a source or a stored layout, and it subscribes to the encoded bus `(picture bus, settings)` like an HLS output. Showing Program at the canvas format with the run's H.264 settings costs **+0 encoders**; another feed opens or shares an encoded bus under the existing admission (`capacity`). Every device that casts the same cast output reads the same files.

2. **Its own packaging, written by the engine in pure Rust.** HLS with CMAF (fMP4) segments; video and one audio rendition as separate CMAF tracks; 2 s segments cut at an IDR (two 1 s GOPs of the shared encode, so no encoder setting changes) under a fixed `EXT-X-TARGETDURATION` of 4 s; a 10-segment window; `CODECS`, `RESOLUTION`, `FRAME-RATE`, `BANDWIDTH` in the master from the real parameter sets; `EXT-X-PROGRAM-DATE-TIME` on every segment from one anchor per activation; `EXT-X-DISCONTINUITY` only at a real parameter change or a packet gap, with a new `EXT-X-MAP`; `tfdt` is the tick-derived timestamp, never rebased. Program audio only (a routed source or Preview carries its own audio through the existing private audio bus).

3. **Opened on the first cast, closed after the last.** The sink is gated. Inactive, it writes nothing and keeps a bounded 20 s pre-roll of encoded packets in memory, so the first cast publishes a full window at once. The directory is removed when the last stream token ends (10 s linger).

4. **A custom Web Receiver, hosted once for every install.** The receiver (CAF v3, three plain static files, strict CSP) is registered as Cast application `8E05190C` with the receiver URL `https://cast.conspect.studio/` (hosted by the `conspect-studio/web` repository, byte-identical copies of `crates/multiview-control/src/cast_receiver/`). It accepts a LOAD only for `<https origin>/cast/<token>/master.m3u8` (plain http only on its own origin), sets the HLS segment formats and `LIVE`, tunes Shaka for a 2 s / 10-segment live stream, shows branded idle, connecting and error screens, retries with a bound, and reports state, errors and playback statistics to the senders (custom namespace `urn:x-cast:studio.conspect.cast`) and to the stream's own engine (`POST /cast/<token>/report`). The engine also serves the same files at `/cast/receiver/` for an air-gapped or self-registered install (`[control] cast_app_id` / `MULTIVIEW_CAST_APP_ID` name the application id the senders launch). With no application id configured the sender uses Google's Default Media Receiver, which the proof shows plays the same stream.

5. **Scoped stream tokens.** Starting a cast mints a 256-bit token stored as SHA-256, valid for the files of one cast output only, in the URL path. It is revoked on stop, expires 90 s after creation if never fetched, 60 s after the last playlist fetch, and after 12 h at most; 32 live tokens, 8 per principal. Creation authorizes both ends exactly as an output take does, and every later change of the output's feed re-checks each live token's owner atomically with a per-output authorization generation that the token routes enforce (decision 6). Token routes are `Cache-Control: no-store`. The operator's API key never leaves the browser; the token never appears in a list, an audit record, an event or a log.

6. **Sender.** The web UI casts from the browser (Cast picker) and, when the engine shares the LAN with the device, from the engine (ADR-M011 session actor, now launching the same receiver and loading the same kind of token URL). The operator picks what to show, sees the receiver's real state and reason, sets volume, stops. Changing what a cast shows is a take on the cast output; the device keeps its URL while the token's owner may read the new feed. A take to a feed the owner may not read revokes the token before the switch.

7. **Proof is part of the change.** A pure-Rust validator checks a cast directory as a player reads it (durations against the target, tfdt continuity, IDR at every segment start, CODECS against the init segments, audio/video alignment). `tools/cast-proof/run.sh` starts the real engine, creates a stream through the API and plays it for 60 s in Shaka 4.15.56 in headless Chrome, and through the real CAF receiver SDK with the receiver app against an emulated Cast platform channel, then checks takes, revoke and authorization on the engine's own routes. The CI job `cast-proof` gates on the cast pipeline and chaos tests and runs the playback proof at 960x540; on the 4 vCPU hosted runner the playback result is reported, not gating, because no size passed 5 of 5 on 4 cores (measured; numbers in `tools/cast-proof/README.md`). The full-size proof needs a host with 8 or more cores. A real-device test by the operator precedes the merge.

## Rationale

- **Why a separate packaging and not a fixed HLS output.** The HLS output serves desktop players and recorders with low latency (1 s segments) and selectable discrete audio tracks; changing it changes every existing consumer. A Cast device wants the opposite trade: a deeper window and longer segments (the receiver buffers 10 s), one audio track, fMP4.
- **Why fMP4 and separate tracks.** It is the path Shaka uses for DASH and CMAF HLS: MSE appends without a JavaScript transmux on a device with a slow CPU. Muxed TS needs the transmuxer; the CAF release notes of 2026 list a fix for "stuck playback on multiplexed TS content".
- **Why 2 s x 10.** With a 2 s target Shaka's presentation delay is 6 s and the window holds 20 s, above CAF's 10 s rebuffering goal even with the Default Media Receiver. Measured: starts in under 1 s with a full window, 0 stalls in 60 s, 6.6 s behind live.
- **Why pure Rust boxes.** The default build has no libav; the packager and the validator run in every CI job. The engine already stamps exact integer timestamps, which map to `tfdt` without a muxer's rebasing.
- **Why a token in the path.** A Cast device cannot send a header; a query string is dropped by relative URL resolution inside playlists; a path prefix scopes every relative segment URL to the same token.
- **Why one hosted receiver.** A Cast application id points at one receiver URL and an unpublished app only loads on registered devices; one published Conspect receiver lets every install cast without its own Google registration. The receiver holds no secret and trusts nothing but the token URL it is given; the media and reports stay on the engine's origin, with CORS that reflects the receiver's origin. The engine-served copy (`/cast/receiver/`) keeps a self-hosted path and is the copy the CI proof tests.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| Tune the existing HLS output (longer segments, bigger window) | Raises latency for every desktop viewer; keeps TS, demuxed discrete tracks and no CODECS; still unscoped and public |
| Default Media Receiver only | No diagnostics, no error reason, no retry policy, no branded idle screen, CAF's defaults; kept as the fallback before registration |
| DASH instead of HLS | No advantage on Cast (same Shaka path with CMAF HLS); a second manifest generator; HLS reuses the existing playlist builders and delivery route |
| Muxed audio + video in one fMP4 or TS rendition | Shaka splits muxed content through its transmuxer; separate CMAF tracks avoid it |
| LL-HLS parts for lower latency | The Cast receiver runs Shaka with low-latency mode off; parts add requests and no benefit |
| A new encode per cast with a longer GOP | Costs an encoder session per cast; two shared 1 s GOPs give the same 2 s segment |
| On-demand add/remove of outputs | Outputs are fixed per run on `main`; a gated sink gives "open on first cast, close after the last" without a topology change |
| WebRTC / Cast Streaming to the device | Chrome-only mirroring stack, a re-encode per device |
| Signed, stateless URLs (HMAC + expiry) | Cannot be revoked on stop; the brief requires revocation |

## Consequences

- **Invariant #1 / #10.** Nothing new runs on the output clock or the bake consumer. The cast sink is one more sink thread behind the bounded per-binding delivery with a drop-when-full policy; a dropped packet becomes a declared discontinuity. Disk I/O happens only on that thread. Chaos tests, under the live drop-on-overload policy: a cast sink blocked in a write, one that panics, and one that stalls cost the clock zero ticks (tick progress sampled while the fault is installed) and the HLS binding zero packets; negative controls that stall the shared consumer fail the same assertions.
- **Invariant #7.** One encode per `(picture bus, settings)`; a cast output adds none for Program.
- **Limits recorded honestly.** A cast output needs H.264 and a picture of at most 1920x1080 at 30 fps. On a larger canvas it declares `[outputs.format]` (ADR-0110 slice B), which gives it its own encode (+1 session, admitted like any other); without a format, on a canvas within the limit, it shares the Program encode. A canvas above 30 fps cannot carry a cast output until per-output frame rates exist (#343); config validation says so. Outputs are fixed at start, so the number of simultaneous different pictures on Cast devices is the number of declared cast outputs.
- **Operations.** The hosted receiver `8E05190C` serves every install; until it is published, only registered test devices load it. An install may register its own app pointing at its engine's `/cast/receiver/` (application id in `[control] cast_app_id` or `MULTIVIEW_CAST_APP_ID`). Receiver changes ship twice (engine copy and hosted copy) and must stay byte-identical (sha256 recorded in both repositories). Cast devices need a publicly resolvable host name and a certificate chain their firmware trusts.
- **Drift risk** (as ADR-M011): the receiver SDK and its Shaka version are Google's. The CI proof pins Shaka to the version the SDK loads and fails when the SDK changes behaviour.
- **The `/hls/` cast path of ADR-M011 remains** for a run with no cast output, unchanged and still best-effort.
