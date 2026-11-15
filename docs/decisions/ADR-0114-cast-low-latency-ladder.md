# ADR-0114: Cast low-latency transport ladder: SRT in a Cast Connect Android TV app, MoQ then WebRTC in the Web Receiver, HLS as the floor, one cast token for every transport

- **Status:** Proposed (design draft; no code in this change)
- **Area:** Streaming / Output / Management / Web/API / Android (invariants #1, #7, #10)
- **Date:** 2026-11-15
- **Amends:** [ADR-0111](ADR-0111-cast-output.md) (adds transports beside its CMAF HLS stream; its stream, token
  rules and receiver stay the floor), [ADR-M011](ADR-M011.md) (the engine-side LAUNCH gains app types)
- **Builds on:** [ADR-0039](ADR-0039.md) (SRT, native listener leaf), [ADR-0049](ADR-0049.md) (WHEP serve),
  [ADR-0107](ADR-0107-single-front-door-443.md) (UDP and TCP 443), [moq-browser-preview.md](../architecture/moq-browser-preview.md)

## Context

A cast today plays the ADR-0111 stream: CMAF HLS, 2 s segments, 10 listed. The receiver holds 6.5 s of delay by
design. In the receiver harness the player reported 6.4 to 9.8 s behind live (`cast_receiver/receiver.js:54-60`).
That is a player figure in headless Chrome, not glass-to-glass on a device. No glass-to-glass number exists for
any transport of this product.

The goal is the lowest latency each device can reach, with the same Cast application id `8E05190C` and the same
sender buttons, and HLS still working everywhere.

### How to read the evidence

Each fact below carries one of these labels.

| Label | Meaning |
| --- | --- |
| Measured | Run by the author on 2026-11-15 (UTC 14 Nov evening); command or probe named |
| Read | Read in the code at `origin/main` f08f9b6a, with path and line |
| Cited | A primary source, linked |
| Reported | A third party's statement, linked; not checked here |
| Unverified | Not established. The slice that settles it is named |

### What limits the design

1. **A browser cannot open a UDP or SRT socket.** Cited: Chrome's Direct Sockets API is available only to
   Isolated Web Apps ([Chrome for Developers](https://developer.chrome.com/docs/iwa/direct-sockets)). A Cast Web
   Receiver is an ordinary web page ([Web Receiver overview](https://developers.google.com/cast/docs/web_receiver)).
   SRT therefore needs a native app.
2. **The Web Receiver's browser is old on older devices.** WebTransport shipped in Chrome 97
   ([Chrome 97](https://developer.chrome.com/blog/new-in-chrome-97)); WebCodecs in Chrome 94
   ([blink-dev](https://groups.google.com/a/chromium.org/g/blink-dev/c/6knQoJRpje4)). The MoQ player on `main`
   needs both (Read: `web/src/preview/moqPlayback.ts:79-89`). Reported: a first-generation Chromecast identifies
   as Chrome 70, `CrKey/1.36` ([one receiver author](https://github.com/renomike/thoth-cast-receiver),
   [another](https://github.com/jaupi-enrico/cast-receiver)); a user-agent list shows Chrome 92 with `CrKey/1.56`
   on a smart speaker ([user-agents.net](https://user-agents.net/devices/tvs/chromecast)). Both are below 94.
   Google publishes no version table. Unverified for every device this product will meet: the receiver's Chrome
   version, and whether WebTransport, WebCodecs and `RTCPeerConnection` work there. The engine has no record of
   the operator's Chromecast user agent that the author could find (Measured: no `CrKey` line in the engine
   journal on mda001 since 13 Nov; nginx is no longer installed there). Slice 1 records it.
3. **One video decoder.** A Chromecast decodes one stream at a time (ADR-0111 decision 8 in lane cast2). Two
   transports cannot play side by side during a switch.
4. **The shared encode already suits low latency in structure.** Read: every encoder opens with B-frames off
   (`crates/multiview-ffmpeg/src/encode.rs:149`), the Program GOP is one second
   (`crates/multiview-cli/src/pipeline.rs:3413`), audio is AAC-LC 48 kHz stereo (`pipeline.rs:4558`).
   Unverified: the encoder's own delay. `program_h264_options` sets only the profile
   (`crates/multiview-ffmpeg/src/encode_options.rs:222-234`); it does not ask for a zero-delay mode. Slice 1
   measures tick-to-packet delay, which bounds every rung.

## Decision

### 1. Device classes

| Class | Devices | Receiver runtime | Native app |
| --- | --- | --- | --- |
| A | Chromecast 1st to 3rd generation, Chromecast Ultra, Nest Hub and Hub Max, Cast speakers, Cast built into a TV that does not run Android | Web Receiver only | No |
| B | Chromecast with Google TV, Google TV Streamer, Android TV sets (Sony, TCL, Hisense, Philips), Nvidia Shield | Web Receiver, and a Cast Connect Android TV app when installed | Yes |
| C | Anything launched without the Conspect receiver: Google's Default Media Receiver (no application id configured), a third-party Cast-compatible receiver | Not ours | No |

Cited for class B: an Android TV app can act as the receiver through Cast Connect
([Android TV receiver overview](https://developers.google.com/cast/docs/android_tv_receiver)); the Web Receiver
also runs on Android TV, and its user agent there contains both `Android` and `CrKey`
([Web Receiver core features](https://developers.google.com/cast/docs/web_receiver/core_features)).

Decode limits (Cited: [Supported media](https://developers.google.com/cast/docs/media)): Chromecast 1st and 2nd
generation H.264 High level 4.1 (1080p30); 3rd generation and Ultra level 4.2 (1080p60); Nest Hub level 4.1 at
720p; Chromecast with Google TV level 5.1; Google TV Streamer level 5.2. AAC-LC and Opus are listed for audio.
The ADR-0111 envelope (H.264, at most 1920x1080 at 30 fps, AAC-LC) holds for every rung. No rung adds a codec.

Class membership is never decided from a user agent or a model name. The receiver that starts tells the engine
what it is and what it can do (decision 3).

### 2. The ladder per class

| Class | Receiver that runs | Rungs, best first |
| --- | --- | --- |
| A | Web Receiver | MoQ, WebRTC, HLS |
| B, Android TV app installed | Android TV app | SRT, HLS |
| B, app not installed, or the app declined the launch | Web Receiver | MoQ, WebRTC, HLS |
| C | Not ours | HLS, as ADR-0111 |

| Rung | Transport | Media, unchanged from the shared encode unless stated | Needs on the receiver |
| --- | --- | --- | --- |
| SRT | SRT caller to the engine's listener, AES-256 | MPEG-TS, H.264 + AAC-LC | Native code: the Android TV app |
| MoQ | WebTransport (QUIC, UDP 443) | H.264 + AAC-LC frames, decoded with WebCodecs | `WebTransport`, `VideoDecoder` and `AudioDecoder` accepting the stream's exact configurations |
| WebRTC | WHEP-style offer and answer; SRTP on UDP 443, or ICE-TCP on TCP 443 | H.264; audio re-encoded once per cast output to Opus | `RTCPeerConnection` with H.264 receive |
| HLS | ADR-0111 stream | CMAF H.264 + AAC-LC | Media Source Extensions (every Cast device) |

Why MoQ sits above WebRTC: it carries the shared encode as it is, including AAC and every audio track of the
output; it needs no extra audio encoder and no per-peer ICE and DTLS state. WebRTC is the rung that still works
when UDP is blocked.

**The Android TV app has two rungs, not four.** A native MoQ or WebRTC player is a second and third media stack
in the app. Instead, when SRT cannot connect, the app plays HLS for that session and remembers the failure for
15 minutes. During that time its launch checker declines Cast launches for that engine, and the Cast platform
loads the Web Receiver, which has the MoQ and WebRTC rungs. Cited: "If a request is rejected, the Web Receiver
is loaded instead of launching natively into the ATV app"
([Android TV receiver core features](https://developers.google.com/cast/docs/android_tv_receiver/core_features)).

#### Selection

The sender does not choose a rung and does not change. Its LOAD stays `<origin>/cast/<token>/master.m3u8`.

1. The receiver that started (web or Android) validates the LOAD URL as today.
2. It posts its capabilities to `POST /cast/<token>/ladder` (decision 3) and gets back an ordered list of offers.
   An engine without this ADR answers 404; the receiver then plays HLS. A receiver without this ADR never asks
   and plays HLS. Old and new copies therefore work together in every combination.
3. It tries the offers in order. Each attempt has a deadline to the first presented frame:

   | Rung | Deadline | Then |
   | --- | --- | --- |
   | SRT | 2.0 s (connect timeout 1.5 s) | HLS in the app; remember the failure |
   | MoQ | 3.0 s (1.5 s to an open session, as [front door](../architecture/network-front-door.md) section 12) | WebRTC |
   | WebRTC | 3.0 s | HLS |
   | HLS | ADR-0111 rules | error screen, as today |

4. The receiver fetches `master.m3u8` once at the start in every case (it already does, to detect a dead token),
   so a fall to HLS starts from a known-good URL.
5. A rung that failed is skipped for 15 minutes on that device for that engine origin (`localStorage` in the Web
   Receiver, app preferences on Android TV). This bounds the worst first start on a network without UDP to one
   failed attempt per rung, once.

#### Live fallback

One decoder means a switch is always visible. The rules keep switches rare and one-directional.

- **Step down** when, on the playing rung: no frame is presented for 2 s; or the transport closes; or the
  receiver's latency estimate (decision 2, Measurement) stays above three times the rung's target for 10 s and
  catch-up did not bring it back. The last frame stays on screen; the busy indicator shows after 1 s; no error
  screen unless HLS also fails.
- **Never step up inside a picture.** A higher rung is tried again only at the next LOAD (a new cast, or the
  sender's video switch between cast outputs) or the receiver's own reload, and only after the 15 minute memory
  has expired.
- **A revoked or expired token is not a fallback.** The receiver asks `master.m3u8` as today; on 404 it ends the
  cast. No rung is tried after that.
- **A take is not a failure.** When the cast output's feed changes, the engine either keeps the attachment
  (same encoder) or ends it with reason `feed-changed`; the receiver then re-attaches on the same rung without
  counting a failure.
- **Tracks (#418).** A rung is offered only if it carries what the session needs. HLS carries every audio and
  subtitle slot. MoQ and SRT carry every audio slot and no WebVTT subtitles in the first version; WebRTC carries
  one audio track. When the operator enables a subtitle track, or picks an audio track the playing rung does not
  carry, the receiver moves to the best rung that carries it (HLS for subtitles). This is an operator action and
  counts as neither failure nor memory.

#### Latency targets

These are targets to test against. None is a measurement.

| Rung | Target, engine tick to picture on the TV | Main terms of the budget |
| --- | --- | --- |
| SRT | at most 0.7 s | encoder delay; SRT receive latency 250 ms (libsrt default is 120 ms, [socket options](https://github.com/Haivision/srt/blob/master/docs/API/API-socket-options.md)); player buffer 100 ms; decode; TV |
| MoQ | at most 1.0 s | encoder delay; player jitter buffer; WebCodecs decode on the device; TV |
| WebRTC | at most 1.0 s | encoder delay; jitter buffer; decode; TV |
| HLS | 6 to 10 s (today's player figure) | 6.5 s presentation delay by design |

Reported elsewhere, for scale only: WebRTC under 500 ms ([FastoCloud](https://fastocloud.com/blog_news/Low_Latency_Streaming_Protocols_Compared_WebRTC_vs_SRT_vs_LL_HLS_for_Live_Operators.html));
WebTransport with WebCodecs under 400 ms at 1080p on a desktop ([webrtcHacks](https://webrtchacks.com/webcodecs-webtransport-and-webrtc/)).
A Chromecast is far slower than a desktop, so those numbers do not transfer.

Joining costs up to one GOP (1 s) on every rung, because no rung may ask the shared encoder for a keyframe
(decision 6). The targets apply after the join.

A rung that measures worse than the rung below it on a device class is removed from that class's ladder in the
engine's offer rules (slice 7). The order above is the starting order, not a promise.

#### Measurement

Two numbers, kept apart.

1. **Tick-to-present (automatic, every cast).** The ladder answer gives the wall-clock time of PTS zero. The
   receiver estimates its clock offset to the engine from the timestamps on its report exchange (four
   timestamps, as NTP does; error at most half the round trip). For each presented frame it knows the PTS
   (`requestVideoFrameCallback` in the Web Receiver, the frame-rendered callback of `MediaCodec` on Android).
   It reports the median and the 95th percentile every 5 s. This number excludes the TV's own processing and is
   therefore not glass-to-glass. It is what the fallback rule uses.
2. **Glass-to-glass (device test, by the operator).** Slice 1 adds a latency slate: a test source that draws the
   engine's wall clock with milliseconds and the tick number in every frame. The web UI gets a reference clock
   page that shows the same engine clock, corrected by the browser's measured offset. One photograph (or a
   240 fps phone video) that contains the TV and a laptop showing the reference page gives the latency as the
   difference of the two readings. Error: one camera exposure, plus the laptop's display delay, plus the offset
   error shown on the page. This is the burned-in clock method (Reported:
   [Fora Soft](https://www.forasoft.com/learn/video-streaming/articles-streaming/latency-glass-to-glass-explained),
   [AWS](https://builder.aws.com/content/3HSKUExqs1fPmQLt7pX5Kk1Qvjp/benchmarking-glass-to-glass-latency-for-amazon-ivs-real-time-streaming)).
   Here "glass" at the sending end is the engine's tick, because the picture is generated, not filmed.

The ADR moves to Accepted only with a table of both numbers per rung and device class (slice 7).

### 3. The capability probe and the ladder endpoint

One new token route pair carries the probe, the offers and the failure reports. It sits beside the report route
(`/cast/{token}/{file}`, Read: `crates/multiview-control/src/routes/cast_public.rs:58-61`).

`POST /cast/<token>/ladder`, JSON, at most 2 KiB, at most one per second per token (burst 4), `no-store`, the
same CORS as the report route.

```json
{ "v": 1,
  "receiver": { "kind": "web", "build": "<sha256 prefix of receiver.js>", "ua": "<at most 200 chars>" },
  "caps": {
    "wt": true,
    "vdec": "supported", "vdec_hw": true, "adec": "supported",
    "rtc": true, "rtc_h264": ["42e01f", "640c1f"], "rtc_opus": true,
    "rvfc": true, "srt": null },
  "need": { "subtitles": false, "audio": null },
  "skip": { "moq": 540 },
  "failed": null }
```

- `wt`: `typeof WebTransport === "function"`. It says the API exists, nothing about the network.
- `vdec`, `adec`: the result of `VideoDecoder.isConfigSupported` and `AudioDecoder.isConfigSupported` for the
  exact codec strings and picture size of this stream. The receiver knows them before it asks, from the
  `master.m3u8` it already fetches (Read: `receiver.js:735`). Values: `supported`, `unsupported`, `absent`.
  `vdec_hw`: the same check with `hardwareAcceleration: "prefer-hardware"`.
- `rtc_h264`: the `profile-level-id` values of `RTCRtpReceiver.getCapabilities("video")`, at most 8.
- `rvfc`: `requestVideoFrameCallback` exists (needed for the latency estimate).
- `srt`: on Android TV, the libsrt version the app loaded; `null` in the Web Receiver.
- `need`: what the session must carry (decision 2, Tracks).
- `skip`: rungs in the device's failure memory, with the seconds left.
- `failed`: on a later call, `{ "rung": "moq", "stage": "connect|first-frame|stall|latency", "ms": 3000 }`.

Every field is a claim by an unauthenticated device. The engine uses it only to leave rungs out. It never widens
access because of it, and it validates sizes and types strictly (unknown fields ignored, wrong types 400).

The answer:

```json
{ "v": 1,
  "clock": { "pts_zero_unix_us": 1794700000000000, "server_unix_us": 1794700012345678 },
  "attach": { "rung": "moq", "url": "https://<host>/moq?...", "broadcast": "...", "protocol": "...",
              "video": { "codec": "avc1.640029", "coded_width": 1920, "coded_height": 1080, "description": "..." },
              "audio": [ { "track": "...", "codec": "mp4a.40.2", "sample_rate": 48000, "number_of_channels": 2 } ],
              "expires_in_ms": 30000 },
  "then": ["webrtc", "hls"],
  "srt": null }
```

- `attach` is the best rung the engine will serve this receiver now, already admitted. `then` names the rest.
  The receiver gets the next one by calling again with `failed` set; that call is also the failure report.
- For `webrtc`, `attach` is `{ "rung": "webrtc", "whep": "whep" }`: the receiver posts its SDP offer to
  `POST /cast/<token>/whep` (`application/sdp`, at most 64 KiB as `routes/whep_output.rs:30`) and gets the
  answer; `DELETE /cast/<token>/whep` ends it. The answer carries the UDP 443 host candidates and the passive
  ICE-TCP candidates on TCP 443 together (Read: [front door](../architecture/network-front-door.md) "Slice 4
  PR2"), so a network without UDP costs no extra round trip.
- For `srt`, `attach` is `{ "rung": "srt", "host": "<public host>", "port": 9010, "latency_ms": 250 }`. It holds
  no secret: the app derives the stream id and the passphrase from the token (decision 5).
- For `hls`, `attach` is `{ "rung": "hls" }`.

What the engine leaves out, in this order: rungs the build or the run cannot serve (no MoQ provider, no SRT
listener); rungs disabled in `[cast] ladder`; rungs the caps exclude;
rungs that cannot carry `need`; rungs in `skip`; rungs removed for the receiver kind by measurement (slice 7).

The engine keeps the last probe and the playing rung per stream (bounded, like the report) and shows them to the
stream's owner and to admins in `GET /api/v1/cast/streams/{id}`: `receiver.caps`, `transport.rung`,
`transport.since`, `transport.fallbacks[]` (at most 8, each with rung, stage, time), `latency_ms.p50/p95`. The
sender's cast card shows the rung and the latency. Every cast on a real device therefore adds to the evidence
of decision 1 without any step by the operator.

**Old devices never parse new code.** `receiver.js` stays ES5 and stays the only script a device parses at
start (Reported: on Chrome 70 one unknown syntax form stops the whole script silently,
[cast-receiver notes](https://github.com/jaupi-enrico/cast-receiver)). The probe is ES5 feature tests. The MoQ
player and the WebRTC player are separate script files that `receiver.js` adds to the page only after the probe
passed and the engine answered with that rung.

**Cast media status in the Web Receiver is the open risk of the MoQ and WebRTC rungs.** CAF builds the media
status that senders and Google Home show from its own player. MoQ draws on a canvas and WebRTC plays a
`MediaStream`; neither goes through CAF's player. Cited: a LOAD interceptor may return `null` so that CAF does
not handle the request, and `PlayerManager` has `setMediaElement`, `setMediaInformation` and `broadcastStatus`
([PlayerManager](https://developers.google.com/cast/docs/reference/web_receiver/cast.framework.PlayerManager)).
Unverified: whether any combination of these gives senders a correct, stable media session for a picture CAF
does not play. Slice 3 settles it first, in the existing harness with the real CAF SDK (`tools/cast-proof/`),
before any player is written. Pass means: the sender's `loadMedia` settles; the session shows as playing with
the title; volume, mute and stop work; the receiver's own reports arrive. If nothing passes, class A keeps HLS
only, slices 4 and 5 are dropped, and the SRT rung is unaffected. The Android TV app does not have this risk:
Cast Connect takes media status from the app's `MediaSession` (Cited: Android TV receiver core features).

### 4. Network

What the test host serves now (Measured 2026-11-15 00:00 UTC, `ss` on mda001 and probes from the build host):

| Port | Listener | Evidence |
| --- | --- | --- |
| UDP 443 | engine: MoQ, HTTP/3 and WebRTC on one socket | QUIC version negotiation answered on IPv4 and IPv6 |
| TCP 443, TCP 80 | engine: site, API, HLS, cast routes, ICE-TCP (`ice_tcp.enabled = true` in the accepted state) | listening |
| UDP 9000 | engine: the SRT output `srt-multiview` (HEVC, listener) | SRT induction answered on IPv4 and IPv6 |
| UDP 9001 to 9100 | nothing | no answer on 9001 |
| UDP 8189 | nothing | not listening. WebRTC moved to UDP 443 with ADR-0107 slice 1 |

Two limits on that evidence. The probes ran from the build host, not from a viewer's network. And the brief for
this ADR says the estate firewall forwards only UDP 9000 to 9100 and 8189; the lead's office probe of 14 Nov
16:24 UTC shows the Program monitor playing over MoQ from the office network at 30 frames per second (Read:
`lead/office-main-5192c285.json`), which needs UDP 443 to pass. The firewall rule itself was not read. No Cast
device's network path to UDP 443 or to the SRT range has been tested (Unverified; slices 4 and 6).

Decisions:

1. **MoQ and WebRTC use the front door as it is**: UDP 443 and TCP 443 on the public host name. No new port.
2. **The engine is the SRT listener; the TV app is the caller.** The engine has the public address; the TV is
   behind NAT. One dedicated listener for every cast output: `[cast.srt] port`, default 9010, dual-stack `[::]`.
   On mda001 9010 is inside the existing SRT grant (9000 to 9100) and is free. It is not the port of any
   configured SRT output: those have one passphrase per output and their own caller limit.
3. **Streams are told apart by stream id, on one port.** The listener installs libsrt's listen callback, which
   receives the caller's stream id before the connection is accepted and sets that connection's passphrase
   (Cited: [access control](https://github.com/Haivision/srt/blob/master/docs/features/access-control.md),
   [`srt_listen_callback`](https://github.com/Haivision/srt/blob/master/docs/API/API-functions.md)).
   `multiview-srt-sys` does not bind that function today (Read: the function table,
   `crates/multiview-srt-sys/src/lib.rs:139-156`); slice 6 adds it, with `srt_setrejectreason` and
   `srt_getversion`.
4. **The cast listener starts only on libsrt 1.5.7 or later.** Cited: advisory
   [GHSA-m96h-3rc9-xx6h](https://github.com/Haivision/srt/security/advisories/GHSA-m96h-3rc9-xx6h) (published
   15 Oct 2026): a handshake packet overflows a heap buffer in a listener before authentication; affected up to
   1.5.6; fixed in 1.5.7. mda001 runs Debian's 1.5.4-1+deb13u1 (Measured), whose changelog lists patches for
   CVE-2026-55868 and CVE-2026-55869 only. Whether that package is affected was not tested. A new public
   listener must not rest on that doubt, so the engine checks the loaded version and reports
   `cast-srt-libsrt-too-old` instead of listening. The same doubt applies to the listener already on UDP 9000;
   that is outside this ADR and is reported to the lead.
5. **Latency setting.** 250 ms receive latency on both ends at first. libsrt's guidance is a multiple of the
   round-trip time; 250 ms covers a 60 ms round trip four times. The app reports the measured round trip; slice
   7 sets the final value from device data.
6. **SRT is off until configured.** `[cast.srt] enabled = false` by default: an install whose firewall does not
   forward the port would make every Android TV cast pay a failed attempt. The engine cannot test its own
   public reachability, so it shows, per cast output, how many SRT attempts failed at connect in the last hour.
7. **When UDP is blocked** (the viewer's network or the engine's):

   | What fails | What the device does | Rung that plays |
   | --- | --- | --- |
   | SRT port only | App: SRT connect times out at 1.5 s, plays HLS, declines launches for 15 minutes; next cast loads the Web Receiver | MoQ |
   | All UDP | Web Receiver: MoQ session does not open in 1.5 s; WebRTC connects over ICE-TCP on TCP 443 | WebRTC over TCP |
   | UDP, and only real TLS passes on 443 (inspecting proxy) | ICE-TCP fails | HLS. TURN over TLS is ADR-0107 slice 8, not built |

   HLS needs only HTTPS on TCP 443, which the cast needs anyway to load the receiver and the token.
8. **Flood floor for the SRT port.** libsrt answers the first handshake packet with a cookie and keeps no state
   until the caller returns it. The runbook adds a per-source nftables limit for the port, as the UDP 443 floor
   does. The listen callback does a bounded table lookup and nothing else.

### 5. Security

1. **One token, one check, every transport.** Every new route is under `/cast/<token>/`. Each one runs the
   check the media route runs today: the re-authorization barrier, then "live, and armed at the output's
   current authorization generation" (Read: `reauth::on_media` then `CastStreams::serve`,
   `routes/cast_public.rs:175-178`). Slice 2 moves that pair into one function that returns the stream, the
   output and the generation, and every rung's admission calls it. No rung has a credential of its own.
2. **An attachment ends when its token would stop being served.** HLS is checked on every request. A push rung
   delivers without requests, so each attachment registers a fence with the token registry: a flag that stops
   enqueue at once, as the MoQ lease fence does (Read: `MoqFence`, `crates/multiview-control/src/moq.rs:124-130`).
   The registry sets the fence inside the same critical section that revokes or re-arms the token: on stop, on
   expiry, and on every feed change before the change is published (Read: `reauth::before_change`,
   `cast_streams/reauth.rs:109`). Setting a fence is one atomic store; no I/O happens under the lock. Closing
   the socket follows on the transport's own thread.
3. **Feed change (as PR 409).** A token whose owner may read the new feed keeps its attachment; the picture
   changes in place. A token whose owner may not is revoked first and its attachment is fenced before the new
   feed's first packet is admitted.
4. **Engine fallback, fail closed in the data path.** The engine can move an output to another feed by itself.
   For HLS the media route compares the engine's readback on each request. A push rung has no request, so the
   check moves to where windows are handed to the attachment: each window carries the picture bus it was
   encoded from, each attachment holds the set of buses its token is cleared for, and a window from any other
   bus is dropped and the attachment fenced. The control plane then re-authorizes as today and the receiver
   re-attaches if its token survives. This needs the dispatcher to label windows with their bus; that code was
   not read for this draft. Slice 2's design note confirms or replaces the mechanism, and its test is the gate:
   a forced fallback to a feed the owner may not read delivers zero packets of it on every rung.
5. **Token lifetime.** Today a live token expires 60 s after its last playlist fetch (Read:
   `cast_streams/registry.rs:48-50`). A push rung fetches no playlist. A successful report or ladder call from
   a stream that has a live attachment counts as that fetch. A device that vanishes stops reporting; its
   transport times out (SRT peer idle 5 s, QUIC idle, ICE consent) and the token expires 60 s later, as today.
6. **Per-object authorization.** No new route takes an object id from the caller. The token names one stream,
   which names one output. The SRT stream id and the MoQ broadcast name are looked up, never trusted as an
   address: an id that matches no live stream is refused the same way as a wrong passphrase. The new fields of
   `GET /api/v1/cast/streams/{id}` follow the route's existing rule (owner or admin). MoQ and WebRTC admission
   run with the token owner's current principal (Read: `resolve_owner`, `reauth.rs:282`), so the providers'
   own dependency checks see the owner's present grants.
7. **SRT stream id and passphrase come from the token.** With `T` the 32 token bytes:

   ```
   prk        = HKDF-SHA256-Extract(salt = "conspect-cast-srt-v1", ikm = T)
   id         = base64url(HKDF-Expand(prk, "stream-id", 16))        22 characters
   passphrase = base64url(HKDF-Expand(prk, "passphrase", 32))       43 characters
   stream id  = "#!::r=cast/" + id + ",m=request"
   ```

   - The passphrase has 256 bits of entropy and fits SRT's 10 to 80 characters (Cited: `SRTO_PASSPHRASE`,
     [socket options](https://github.com/Haivision/srt/blob/master/docs/API/API-socket-options.md)). Key length
     32 bytes (AES-256), counter mode. GCM is a preview build option in libsrt (Cited, same page) and is not used.
   - The stream id travels in clear in the handshake. It reveals neither the token nor the passphrase: the two
     HKDF outputs are independent. It only selects which passphrase the listener sets.
   - A wrong passphrase is rejected by libsrt itself (Cited: access control). The callback cannot and does not
     compare passphrases.
   - The engine stores nothing new. It can already derive the token from its stored seed and process pepper
     (Read: `cast_streams/token.rs:85-98`), so it derives `id` and `passphrase` when the stream is created and
     keeps them in the registry entry, whose `Debug` prints no secret.
   - One SRT attachment per token. A second authenticated caller with the same id replaces the first (a
     reconnect). At most 4 failed handshakes per id per minute; then the id is refused for a minute.
8. **No secret on argv or in logs.** The passphrase goes to libsrt through the socket option
   (`ListenerOptions::with_encryption`, Read: `multiview-srt-sys/src/lib.rs:350`), never through a `srt://` URL,
   a libav option string or a process argument, on the engine and in the app. The MoQ lease URL is marked "never
   log" (Read: `moq.rs:77`) and stays so. The ladder answer is `no-store`. Logs carry a token's first 6
   characters at most, as `receiver.js` does today. The engine's SRT link status shows the peer address and
   counters, never the stream id.
9. **Bounds.** Ladder calls: 1 per second per token, burst 4, body 2 KiB. Attachments: one per token per rung,
   so at most 32 per rung (the token cap, Read: `MAX_STREAMS`, `cast_streams/mod.rs:46`). WHEP offer 64 KiB.
   Stored probe and fallback history: fixed size per stream.
10. **The receiver page.** The policy already allows `connect-src 'self' https:` and `worker-src blob:` (Read:
    `RECEIVER_CSP`, `routes/cast_public.rs:36`). Unverified in a browser: that this admits a WebTransport
    session to another `https` origin. Slice 3 checks it in the harness. The hosted copy at
    `cast.conspect.studio` must send the same policy and the same new files, byte for byte.

<!-- NEXT -->
