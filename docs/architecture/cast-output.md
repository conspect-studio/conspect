# Cast output: a stream a Google Cast receiver plays

Design note for [ADR-0111](../decisions/ADR-0111-cast-output.md). It states what is built, the numbers behind each
choice, and how it is proven. Code: `crates/multiview-output/src/hls/{cmaf,cast,cast_check}.rs`,
`crates/multiview-cli/src/cast_output.rs` (+ `cast_output/`), `crates/multiview-control/src/{cast_engine,cast_streams}.rs`,
`crates/multiview-control/src/routes/cast_streams.rs`, `crates/multiview-control/src/cast_receiver/`, `web/src/cast/`,
`tools/cast-proof/`.

## 1. What a Cast receiver does with HLS (measured)

Measured with Cast Web Receiver SDK 3.0.0157 and Shaka Player 4.15.56 (the version the SDK loads) in headless
Chrome; scripts and logs are in `tools/cast-proof/`.

| Fact | Source |
| --- | --- |
| The SDK plays HLS with Shaka and sets `streaming.rebufferingGoal = 10` s, `stallThreshold = 5`, `jumpLargeGaps` | SDK source, `shakaConfig` merge |
| Shaka holds the play head 3 target durations behind the newest segment | Shaka `hls.liveSegmentsDelay` |
| Shaka never appends MPEG-TS natively; it transmuxes TS in JavaScript | Shaka `polyfill/mediasource.js` |
| A 1.001 s x 6 MPEG-TS stream with separate audio (the HLS output): 3 s seek range, `seeking -> waiting` loops; with the SDK defaults no picture advance in 40 s on a generated stream, picture after 7 to 15 s on the live test host | harness, "production-shaped" runs |
| A 2 s x 10 CMAF stream: picture in under 1.5 s with the Conspect receiver, 4 to 7 s with the SDK defaults, 0 stalls in 60 s, 7 to 10 s behind live | harness |
| A playlist that lists fewer than 5 segments starts slowly with the SDK defaults (it waits to buffer 10 s in real time) | harness, cold-start cases |

Not measurable without a device: the hardware decoder's limits, the firmware's Chrome version, the firmware's
trust store. The stream therefore stays inside the documented envelope of the oldest supported devices: H.264
High at most level 4.1 class (1920x1080 at 30 fps), AAC-LC stereo 48 kHz.

## 2. The stream

```
master.m3u8
  #EXT-X-VERSION:7
  #EXT-X-INDEPENDENT-SEGMENTS
  #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="Program",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="audio.m3u8"
  #EXT-X-STREAM-INF:BANDWIDTH=..,AVERAGE-BANDWIDTH=..,CODECS="avc1.640029,mp4a.40.2",RESOLUTION=1920x1080,FRAME-RATE=29.970,AUDIO="aud"
  video.m3u8
video.m3u8 / audio.m3u8
  #EXT-X-TARGETDURATION:4, #EXT-X-MEDIA-SEQUENCE, #EXT-X-MAP:URI="init-v-<gen>-<k>.mp4"
  #EXT-X-PROGRAM-DATE-TIME + #EXTINF:2.002000 + seg-v-<gen>-<n>.m4s   (10 listed)
```

- **Segments.** CMAF: `styp` + `moof` (`mfhd`, `traf`: `tfhd` default-base-is-moof, `tfdt` v1, `trun`) + `mdat`. A
  video segment is cut at the first IDR at least 2 s (minus half a frame) after its start: two of the shared
  encode's 1 s GOPs, 2.002 s at 29.97 fps. The audio segment with the same sequence number holds the AAC frames
  between the two video cut points, so both playlists always list the same sequence numbers.
- **Target duration.** `EXT-X-TARGETDURATION` is fixed for a packager's lifetime (RFC 8216 requires it constant
  for a media playlist): twice the nominal segment, 4 s. A segment that would exceed it (no IDR in time) is a
  packager error; the stream restarts with a new generation, i.e. new playlist content and file names. Shaka's
  live delay sums the `EXTINF` of the last three segments, so the larger target does not change latency.
- **Timestamps.** `tfdt` is the tick-derived timestamp in the stream's own time base, never rebased, never a float.
  `EXTINF` is the exact sum of sample durations.
- **Discontinuities.** Only for a real change: new SPS/PPS (a take onto another encoder) or lost packets. Both
  playlists mark the same sequence number and keep equal discontinuity sequences; video gets a new init segment.
- **PDT.** One anchor per activation: the shared output epoch, else the wall clock recorded when the packets
  arrived. It never steps inside an activation.
- **Names.** `<gen>` is unique per activation (at least the Unix time in ms), so a URL is never reused for other
  bytes. The token routes still send `Cache-Control: no-store` on every response, so no shared cache can serve
  a revoked or expired token's URL.
- **Window.** 10 listed; an evicted segment stays on disk for 10 more closes; bounded file count.

## 3. The engine

- `[[outputs]] kind = "cast"` builds a sink like any other, on its own thread, behind the per-binding bounded
  delivery with the drop-and-count policy: a sink that does not take a packet within the delivery's grace loses
  packets (counted, declared as a discontinuity), is never waited for again until it recovers, and never paces
  the bake consumer. Nothing is added to the output clock.
- **Gate.** `CastGate` is atomics on the packet path. Inactive: no file; the last 20 s of packets (whole GOPs,
  at most 32 MiB) stay in a ring in memory. Activation with pre-roll packages the ring at once: a full window and
  the master exist within milliseconds. `ready` = master + 5 video segments.
- **Sharing.** The output's video key is resolved like an HLS output's. No `[outputs.format]` and the same codec
  and bit rate as another output: the same encoder (+0). Its own format: its own encoder (+1). A take to another
  feed uses the routing encoded buses and their admission.
- **Directory.** Default `<state dir>/cast/<id>`, mode 0700. The engine writes and deletes only in a directory that
  was absent, empty, or carries its marker file with this output's id, and deletes only files with the packager's
  own names. Any other directory disables that cast output for the run (the engine still starts).
- **Failure.** A packager error restarts the stream with a new generation at the next keyframe, at most once per
  second. A panic in the sink disables only that output.

## 4. Control plane

| Route | Auth | Purpose |
| --- | --- | --- |
| `GET /api/v1/cast/config` | Read | receiver app id, namespace, cast outputs and their state, last receiver-page fetch |
| `POST /api/v1/cast/streams` | Write + both ends as an output take | pick or take a cast output, activate it, mint a token |
| `GET /api/v1/cast/streams[/{id}]` | owner or admin | state, `ready`, fetch counters, last receiver report |
| `DELETE /api/v1/cast/streams/{id}` | owner or admin | revoke at once |
| `GET /cast/{token}/{file}` | the token | playlists, init and media segments; CORS reflects the Origin |
| `POST /cast/{token}/report` | the token | receiver state report (2 KiB, 1/s) |
| `GET /cast/receiver/` | public | the receiver app (self-hosted copy) |
| `POST /api/v1/cast/sessions` (`feed`) | Write + as `POST /cast/streams` | the engine itself launches the receiver on a LAN device (ADR-M011) and loads `{cast_media_base}/cast/<token>/master.m3u8`; the session owns the stream; an http base is refused `cast-no-public-base` |

Token rules, limits, expiry and the redaction guarantees are in ADR-0111 decision 5. A take issued by the cast
API has an operation id that starts with `cast:` and is not written to `[[routing.output]]`; when the last
stream of an output ends, the output goes back to Program after a 10 s linger.

**Takes re-authorize tokens.** Every path that changes what a cast output carries (routing and patch takes, the
cast API's own take and take-back, a layout edit or delete, the audio-routing PUT) checks each live token on that
output against its owner's current grants for the new feed's whole closure (source, layout cells and audio,
audio routes) before the change is published. A token whose owner may not read the new feed, or whose owner no
longer resolves, is revoked first: its next request is 404, and the owner's `GET` shows `cast-stream-revoked` with
the reason. Engine fallbacks and a 5 s sweep (which also catches a revoked or re-scoped API key) are backstops.

**Diagnosis without a device log.** The engine records, per stream and for the receiver page, the last fetch
time and User-Agent (a Cast device contains `CrKey`). No fetch from a Cast device after a cast starts means the
device never completed DNS or TLS to the host; fetches without a `playing` report mean a playback fault, and
the report carries the reason.

## 5. The receiver

Three static files (ES5, strict CSP, HTTPS only), in `crates/multiview-control/src/cast_receiver/`. The
registered Conspect receiver is application `8E05190C` at `https://cast.conspect.studio/` (byte-identical copies,
hosted by `conspect-studio/web`); every engine also serves them at `/cast/receiver/` for a self-registered or
air-gapped install. It accepts only `<https origin>/cast/<token>/master.m3u8` (http only on its own origin) and
posts reports to that stream's origin; sets `LIVE`, fMP4 formats and its Shaka settings (`rebufferingGoal 4`,
`bufferingGoal 12`, `defaultPresentationDelay 6.5`); shows idle, connecting, buffering and error screens; reports
every state change and every 5 s; retries a fatal error after 2, 4 and 8 s; detects a revoked token by fetching
the master (Shaka retries a live 404 for ever) and then closes itself; keeps playing when the sender's tab closes.
The engine reports the application id the senders launch as `receiver_app_id` (`MULTIVIEW_CAST_APP_ID` wins over
`[control] cast_app_id`). Without one the sender uses Google's Default Media Receiver, which plays the same stream
with Google's defaults and reports nothing. CSP of the receiver page: `media-src 'self' https: blob:` and
`connect-src 'self' https: ws://localhost:8008` (the last is the device's platform socket).

## 6. Proof

1. `multiview_output::hls::cast_check` + `cargo run -p multiview-output --example cast_validate -- <dir>`:
   a strict reader of a cast directory. Unit and integration tests run in the default build.
2. Pipeline and chaos tests (`crates/multiview-cli/src/cast_output/pipeline_tests.rs`, real clock, libx264):
   inactive writes nothing; pre-roll; validity; take and back; a blocked sink and a panicking sink cost the
   clock zero ticks and the HLS binding zero packets; a stalled sink declares its gap. The HLS output in the
   chaos tests is the HLS binding (recording sink): the real binding of the configured HLS output, whose sink
   runner records every packet it is handed (video PTS, audio count) instead of packaging HLS. The chaos tests
   run under the live drop-on-overload policy and prove isolation by zero hot-queue shed, every tick emitted,
   contiguous PTS 0..N and the control run's audio count at that binding, and the clock's tick progress sampled
   against its own time while the fault is installed (at least 25 - 5 ticks per second of clock time). Two
   negative controls stall the shared bake consumer and must fail these assertions. The cast binding's drop
   counter is exact: received + dropped = offered, including queued windows flushed on overload or expiry.
3. `tools/cast-proof/`: the real engine, a stream created through the API, 60 s in Shaka 4.15.56 with the SDK's
   defaults and with the receiver's settings, 60 s through the real receiver SDK and the receiver app against an
   emulated platform channel, takes, revoke, authorization and the validator on the directory (fault cases:
   `run.sh --reference`). CI job `cast-proof`: the pipeline and chaos tests gate; the playback proof runs at
   960x540 and is reported, not gating, on the 4 vCPU runner (no size passed 5 of 5 on 4 cores, see the tool's
   README); the full-size run needs 8 or more cores.
4. A device test by the operator (runbook `docs/runbooks/cast.md`).

What the automated proof does not cover is listed in section 1.
