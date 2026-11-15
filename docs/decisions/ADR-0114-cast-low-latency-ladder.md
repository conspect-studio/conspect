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

<!-- NEXT -->
