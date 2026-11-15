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
| MoQ | WebTransport (QUIC, UDP 443) | H.264 + AAC-LC frames, played through a `<video>` element (decision 3, MoQ renderer) | `WebTransport`, and Media Source Extensions accepting the stream's codec strings |
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
| MoQ | at most 1.0 s | encoder delay; the `<video>` element's buffer (150 ms target); decode; TV |
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
    "wt": true, "mse": true,
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

**MoQ renderer: a `<video>` element fed through Media Source Extensions, not a canvas.** The desktop MoQ player
decodes with WebCodecs and paints a canvas (Read: `web/src/preview/MoqPlayer.tsx:93`). On a Chromecast that is
the wrong surface. Cited: "Chromecast displays the receiver page on a 720p graphics plane"
([Web Receiver core features](https://developers.google.com/cast/docs/web_receiver/core_features)); a canvas is
part of the page, so a 1080p picture would be shown at 720p at best, and painted by the page instead of the
video hardware path. The cast MoQ player therefore wraps each received frame in a CMAF fragment and appends it
to a `SourceBuffer`, and the `<video>` element plays it: the same decoder and video plane the HLS rung uses on
that device, which is known to work on the operator's Chromecast. It also removes WebCodecs from the
requirements, which lowers the Chrome version needed from 94 to 97 for WebTransport alone, and the element keeps
audio and video in sync. The cost is the element's buffer: the player holds a small target (start at 150 ms)
and trims by playback rate. Unverified: the latency this reaches on a device; slice 4 measures it. `vdec` and
`adec` stay in the probe so that the data shows which devices could run a canvas player, if that is ever wanted.
The probe gains `mse`: `MediaSource.isTypeSupported` for the stream's codec strings.

**Cast media status in the Web Receiver is the open risk of the MoQ and WebRTC rungs.** CAF builds the media
status that senders and Google Home show from its own player. MoQ draws on a canvas and WebRTC plays a
`MediaStream`; neither goes through CAF's player. Cited: a LOAD interceptor may return `null` so that CAF does
not handle the request, and `PlayerManager` has `setMediaElement`, `setMediaInformation` and `broadcastStatus`
([PlayerManager](https://developers.google.com/cast/docs/reference/web_receiver/cast.framework.PlayerManager)).
Both players here end in a `<video>` element, which is what `setMediaElement` takes. Unverified: whether any
combination of these gives senders a correct, stable media session for a picture CAF did not load itself. Slice 3 settles it first, in the existing harness with the real CAF SDK (`tools/cast-proof/`),
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

### 6. Engine cost and invariants

**Encodes.** No rung adds a video encoder. Every rung of a cast output consumes the encoded packets that
output's binding already receives (invariant #7).

| Rung | Video | Audio | Added cost |
| --- | --- | --- | --- |
| HLS | shared H.264 | shared AAC | none (ADR-0111) |
| SRT | shared H.264 | shared AAC, every audio slot | one MPEG-TS mux per attachment |
| MoQ | shared H.264 | shared AAC, every audio slot | framing per cast output; QUIC send per attachment |
| WebRTC | shared H.264 | **one Opus encoder per cast output**, 96 kbit/s, running only while that output has a WebRTC attachment | RTP and SRTP per attachment |

The Opus encoder is the one real addition. The preview WHEP path opens an Opus encoder per session today
(Read: `crates/multiview-cli/src/whep.rs:19`, `:436`); the cast rung must not copy that. It opens one per cast
output, fed from the output's audio bus through a bounded drop-oldest tap, shared by all of that output's
WebRTC attachments, and closed with the last one. It is declared to the admission the way routed encoders are.

**How a rung gets its packets.** On `main` each configured output's delivery is mirrored, whole window by whole
window, into the effective-output registry, before and independent of the physical sink's own admission (Read:
`crates/multiview-cli/src/pipeline.rs:7487-7519`). MoQ output leases already subscribe there. The cast rungs
subscribe to the cast output's entry the same way. Limits already in place: 16 subscribers per producer, a
two-window handoff per subscriber, and a subscriber that falls behind loses its whole window and resumes at the
next verified keyframe (Read: `output_window_registry.rs:38`, `:43`, module doc). Two things to settle in
slice 2, both Unverified here: that a `cast` output's binding enters that registry (nothing in the registry
names the kind; `binding_ids` covers every configured output, `pipeline.rs:5135-5139`), and that the encoder's
no-reorder proof holds for the production encoder on mda001 (a subscription is refused without it, Read:
`output_window_registry.rs:581`).

**Activation.** A cast output is gated: it packages nothing until a stream activates it. The first ladder
attachment activates the output exactly as the first HLS playlist request does today, and the last one ending
starts the same 10 s linger. A push rung does not use the 20 s pre-roll ring; it starts at the next keyframe.

**Queues (invariant #10).** Every hand-off toward a viewer is bounded and drops:

| Hand-off | Bound | When full |
| --- | --- | --- |
| Dispatcher to a rung's subscription | 2 windows | whole window dropped, discontinuity, resume at a keyframe |
| MoQ subscription | 8 MiB per subscription, 64 MiB in total, 16 sessions (Read: `crates/multiview-moq/src/provider.rs:454-455`, `:867`) | window refused, lease survives or ends by class of error |
| SRT attachment | its own writer thread and a 512-packet queue, as the SRT listener output (Read: `crates/multiview-output/src/sink/push.rs:69`) | shed, then re-gated to the next keyframe |
| WebRTC attachment | the bounded drop-oldest egress feed (Read: `crates/multiview-webrtc/src/transport/whep_serve.rs:22-28`) | oldest dropped |
| Opus tap | fixed block count, drop-oldest | oldest dropped |

No hand-off waits for a viewer. Nothing a device sends (probe, report, SDP, SRT handshake) reaches a data-plane
thread: control routes answer from the registry, and the SRT listen callback reads a bounded table.

**Keyframes.** No rung can request a keyframe from the shared encoder. A WebRTC PLI or FIR is counted and
ignored; the viewer recovers at the next scheduled keyframe, at most one GOP away. One viewer on a bad network
therefore cannot change the bit rate or picture quality of any other output.

**Output clock (invariant #1).** No new work on the clock thread or the bake consumer. Fences are atomics read
on the delivery path. The token registry's lock is taken by control routes only and never across an `.await`
(Read: module doc, `cast_streams/mod.rs:23-24`).

**Risk class.** Slices 2, 4, 5 and 6 add subscribers and sinks next to the data plane: R3. Each brings a design
note and chaos tests of the shape ADR-0111 used: a wedged, a panicking and a stalled attachment cost the clock
zero ticks and every other binding zero packets, with a negative control that must fail the same assertions.

### 7. Cast Connect and the Android TV app

**Launch.** The same application id serves both receivers. Cited
([Android TV receiver core features](https://developers.google.com/cast/docs/android_tv_receiver/core_features)):

- the sender sets `androidReceiverCompatible = true` in its Cast options (Web sender: Chromium 87 or later);
- the Android TV app's package name is added to the application in the Cast Developer Console;
- "Without registration, Cast Connect will only work for apps installed from the Google Play Store": a
  sideloaded app is launched only on a device whose Cast serial number is registered in the Console;
- the LOAD reaches the app as an intent (`com.google.android.gms.cast.tv.action.LOAD`) with the same media
  information; custom namespaces work (`CastReceiverOptions.setCustomNamespaces`, `sendMessage`);
- the app may decline a launch in its `LaunchRequestChecker`, and the Web Receiver is then loaded.

Not stated on that page, and so Unverified: what the platform does when the flag is set and the app is not
installed. The design assumes the Web Receiver loads, as every Cast Connect sender relies on. Slice 6's device
test has that case as its first step, because the flag goes to every sender and a wrong assumption would break
casting to every Android TV. Until that step passes, the flag ships behind `[cast] android_tv = false`.

Sender changes, complete list:

1. Web sender: add `androidReceiverCompatible: true` beside `receiverApplicationId` (Read: the options object,
   `web/src/cast/sdkLoader.ts:81-84`). No button, picker or LOAD change.
2. Engine-side sender (ADR-M011): its LAUNCH is `{type, requestId, appId}` today (Read:
   `crates/multiview-control/src/devices/cast/protocol.rs:161`). Chromium's own sender adds
   `"supportedAppTypes": ["WEB", "ANDROID_TV"]` to that message (Cited:
   [`cast_message_util.cc`, `CreateLaunchRequest`](https://github.com/chromium/chromium/blob/main/components/media_router/common/providers/cast/channel/cast_message_util.cc)).
   The engine sends the same field. Unverified against a device; without it the engine-side cast keeps loading
   the Web Receiver, which is today's behaviour.

No launch credentials are sent. The cast token in the LOAD is the only credential, as in the Web Receiver.

**The app.**

| Item | Decision |
| --- | --- |
| Package name | `studio.conspect.cast.tv` (reverse of the domain that hosts the receiver; `tv` leaves room for a phone app) |
| Source | `android/cast-tv/` in this repository, Kotlin, Gradle. Not a Cargo workspace member: `cargo check --workspace` and the default build do not change |
| Minimum Android | API 23, the floor of `srtdroid`'s libsrt build; raised if Media3 or the Cast TV library needs more (checked at slice 6) |
| Cast | `com.google.android.gms:play-services-cast-tv` (21.1.1 in Google's guide today), `CastReceiverContext`, `MediaManager` bound to a `MediaSession` |
| Player | Media3 ExoPlayer: `ProgressiveMediaSource` over a custom `DataSource` that reads SRT payload, `TsExtractor`, platform `MediaCodec` decoders. HLS fallback through Media3's HLS module in the same player |
| SRT | libsrt 1.5.7 through JNI, from the `io.github.thibaultbee.srtdroid:srtdroid-core` 1.10.1 package, which builds libsrt 1.5.7 with OpenSSL 3.5.1 (Read: its `CMakeLists.txt` at tag 1.10.1) |
| Low latency | live target offset 100 ms, minimum buffer 50 ms, playback speed 0.97 to 1.03 to hold the target; values tuned in slice 7 |
| Namespace | `urn:x-cast:studio.conspect.cast`, the same report shape as the Web Receiver, plus `receiver.kind = "android-tv"` |
| LOAD check | the same rule as `receiver.js`: `https` only, path `/cast/<token>/master.m3u8`, no credentials, no query |
| Secrets | the token is held in memory only; never written to preferences, logs or the `MediaSession` metadata |
| Leanback | declared as a TV app with a launcher banner; opened by hand it shows "Cast from Conspect to this TV" |

**Licences.** No GPL code, and nothing is added to the engine's default build.

| Component | Licence | Source |
| --- | --- | --- |
| libsrt | MPL-2.0 | [Haivision/srt](https://github.com/Haivision/srt) |
| srtdroid (JNI binding) | Apache-2.0 | [ThibaultBee/srtdroid](https://github.com/ThibaultBee/srtdroid) |
| OpenSSL 3.x (libsrt's crypto) | Apache-2.0 | [OpenSSL licence page](https://www.openssl.org/source/license.html) |
| AndroidX Media3 | Apache-2.0 | [androidx/media](https://github.com/androidx/media) |
| Google Play services Cast TV | Google's SDK terms (proprietary, not copyleft) | [Cast SDK terms](https://developers.google.com/cast/docs/terms) |

MPL-2.0 is file-level copyleft: the app ships libsrt unmodified as its own shared library and names where its
source is. `deny.toml` already allows MPL-2.0 for the engine (Read: line 81). The app's notices file is
generated by the build from the dependency list. Writing or changing licence or legal text is not part of
these slices; the list of obligations goes to the lead with slice 6.

Alternatives for SRT on Android, rejected: FFmpeg's libsrt protocol inside the app (a second, large native
dependency, and licence care for the codec set, to do what Media3 already does); a pure-Kotlin SRT client (none
found that is maintained; SRT's encryption and retransmission are not a weekend's work); our own JNI binding to
libsrt (kept as the fallback if `srtdroid` lags a libsrt security release by more than two weeks: the binding
needed is six calls).

**Distribution.** Sideload first: CI builds a debug-signed APK on every pull request and a release-signed APK
on a tag, signed with a key held in 1Password and given to CI as a secret. The operator installs it with `adb`.
Publishing to Google Play is an operator action (account, listing, review) and is not needed for registered
devices. The engine does not serve the APK.

**Version skew.** The app and the engine negotiate through the ladder call (`v`), and the app plays HLS against
an engine that does not answer it. The app shows its version on its idle screen and in every report.

<!-- NEXT -->
