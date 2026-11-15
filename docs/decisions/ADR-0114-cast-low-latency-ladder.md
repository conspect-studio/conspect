# ADR-0114: Cast low-latency transport ladder: SRT in a Cast Connect Android TV app, MoQ then WebRTC in the Web Receiver, HLS as the floor, one cast token for every transport

- **Status:** Accepted as a design on 2026-11-15; no code in this change. Review: round 1 NEEDS_REVISION with
  findings D1 to D4 (OpenAI gpt-6-astra, on 9e292130); round 2 PASS (OpenAI gpt-5.6-terra, on de3f8e15). Accepted
  means the design may be built slice by slice. It does not mean anything here is measured: every latency
  figure is a target until slice 7, and the rungs marked Unverified stay so until their slice closes them
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
   version, and whether WebTransport, WebCodecs and `RTCPeerConnection` work there. The author found no record of
   the operator's Chromecast user agent: the engine keeps the last fetcher's user agent per stream in memory
   only (Read: `CastFetches`, `cast_streams/mod.rs:113-126`), and a search of the engine journal on mda001 for
   `CrKey` since 13 Nov found nothing, which shows only that it is not logged. Slice 1 records it.
3. **One video decoder.** A Chromecast decodes one stream at a time (ADR-0111 decision 8 in lane cast2). Two
   transports cannot play side by side during a switch.
4. **The shared encode already suits low latency in structure.** Read: every encoder opens with B-frames off
   (`crates/multiview-ffmpeg/src/encode.rs:149`), the Program GOP is one second
   (`crates/multiview-cli/src/pipeline.rs:3413`), audio is AAC-LC 48 kHz stereo (`pipeline.rs:4558`).
   Unverified: the encoder's own delay. `program_h264_options` sets only the profile
   (`crates/multiview-ffmpeg/src/encode_options.rs:232-243`); it does not ask for a zero-delay mode. Slice 1
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
The ADR-0111 envelope (H.264, at most 1920x1080 at 30 fps, AAC-LC) holds for every rung. No rung adds a video
codec. The WebRTC rung adds one audio codec, Opus (decision 6).

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
  receiver's `latency_ms` median stays above three times the rung's target for 10 s and catch-up did not bring
  it back. The latency cause applies only while the report's `method` is `frame` or `playhead` (see "Engine
  media time on each rung"); with `unavailable` it is switched off. The last frame stays on screen; the busy indicator shows after 1 s; no error
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

#### Engine media time on each rung

Reviewer finding D1. A presented frame's timestamp is not the engine's timestamp on every rung, so the mapping
is fixed here per rung, and a rung without a trustworthy mapping reports no latency at all.

**Engine media time `E`** is the output's presentation timestamp in microseconds, derived from the tick counter
(exact integer rescale of the encoder time base, never a float). It is one timeline per engine run. Takes and
encoder changes do not reset it. Only an engine restart does, and a restart ends every attachment.

**Wall time of `E`** comes from the pipeline's outbound epoch, the same map that stamps
`EXT-X-PROGRAM-DATE-TIME` (Read: `crates/multiview-output/src/epoch.rs:1-13`; the cast sink holds it,
`crates/multiview-cli/src/cast_output/runner.rs:62-63`). The map has a generation that changes when the epoch
is stepped (Read: `epoch.rs:15-26`). The ladder route returns one pair of that map, never a bare "PTS zero":

```json
"clock": { "source": "epoch", "epoch_generation": 7,
           "media_us": 5400000000, "wall_unix_us": 1794705000000000,
           "t1_unix_us": 1794705012345000, "t2_unix_us": 1794705012345400 }
```

`clock` is `null` when the run has no validated epoch; the MoQ lease already refuses to invent one (Read:
`clock_wall_micros: None`, `crates/multiview-cli/src/moq_outputs/factory.rs:756`). `source` is `arrival` when
the only anchor is the time packets reached the sink (ADR-0111's second choice); a latency computed from it
leaves out the encoder's delay and is labelled so.

**Receiver clock.** The receiver reads time from a monotonic clock only (`performance.timeOrigin +
performance.now()`; `System.nanoTime()` on Android), never from the settable wall clock. It estimates its
offset to the engine from four timestamps of one ladder call, as NTP does: `t0` sent, `t1` received by the
engine, `t2` answered, `t3` received. Offset is `((t1 - t0) + (t2 - t3)) / 2`; its error is at most
`((t3 - t0) - (t2 - t1)) / 2`. It keeps the sample with the smallest error of the last 8 and repeats the call
every 30 s (`{"v":1,"clock":{"t0":...}}`, answered with `clock` only). If the best error is above 250 ms, the
clock is `unavailable`.

**What the receiver reads, per rung.**

| Rung | Timestamp the receiver gets for a presented frame | How it becomes `E` | When the frame was shown |
| --- | --- | --- | --- |
| HLS | Media time on the element's timeline. `tfdt` is the tick-derived timestamp, never rebased (ADR-0111 decision 2) | Not needed: `playerManager.getAbsoluteTimeForMediaTime(t)` returns the wall time from the playlist's date-time (Cited: [PlayerManager](https://developers.google.com/cast/docs/reference/web_receiver/cast.framework.PlayerManager)), which is the epoch's wall time of that frame | `requestVideoFrameCallback`: `mediaTime` and `expectedDisplayTime` |
| MoQ | Each frame arrives with its timestamp in microseconds, the exact rescale of the encoder PTS (Read: `micros`, `crates/multiview-cli/src/moq_outputs.rs:53-66`). No wrap inside 2^62 | The player writes `tfdt = timestamp - B`, where `B` is the first keyframe's timestamp of this attachment, with `timestampOffset = 0` and `mode = "segments"`. `B` is the player's own number, so `E = mediaTime * 1e6 + B` holds by construction | as HLS |
| WebRTC | The RTP timestamp of the frame: `metadata.rtpTimestamp` of `requestVideoFrameCallback` (Cited: "SHOULD be present for WebRTC applications", [video.rVFC](https://wicg.github.io/video-rvfc/)). `mediaTime` is not used: the same text says it "MAY have a zero value for live-streams or WebRTC applications" | Engine contract below; unwrapped against the clock | `expectedDisplayTime` of the same callback |
| SRT | `presentationTimeUs` of Media3's `VideoFrameMetadataListener` (Read: `VideoFrameMetadataListener.java`, Media3 1.8.0) | TS contract below; unwrapped against the clock | `releaseTimeNs` of the same callback, which "can be compared to `System.nanoTime()`" |

**WebRTC contract.** The cast rung writes the wire RTP timestamp as `R = floor(E * clock / 1e6) mod 2^32`, with
clock 90 000 for video and 48 000 for audio, and no offset. Three things make that true and each is a slice 5
requirement, not an assumption:

1. The conversion wraps. The WebRTC output on `main` converts with `u32::try_from(ts.max(0)).unwrap_or(0)`
   (Read: `crates/multiview-cli/src/pipeline.rs:10028-10030`), which yields 0 for every frame once the 90 kHz
   value passes 2^32, 13 h 15 min into a run; the audio conversion has the same form and reaches 2^32 at
   48 kHz after 24 h 51 min. Not reproduced at run time. The cast rung uses wrapping arithmetic from its first
   version: a masked conversion, as the preview path does (Read: `crates/multiview-cli/src/whep.rs:317-320`).
   The existing output is outside this ADR; it is issue #492, fixed by ship lane `rtpwrap`.
2. The library adds no offset. In str0m 0.16.2 the sample's time goes to the header as given (Read:
   `src/packet/payload.rs:58`, `src/streams/send.rs` `write_rtp`). A test reads a sent packet and compares.
3. Audio is stamped with engine time. The preview path stamps Opus with the encoder's own sample counter (Read:
   `whep.rs:315-316`). The cast rung's one Opus encoder per output is seeded from the engine timestamp of the
   audio block it encodes. Unverified: that the audio tap carries that timestamp; if not, slice 5 adds it.

The receiver unwraps `R`: of all values congruent to `R` modulo 2^32 it takes the one nearest to the engine
media time it expects now (from `clock`). That is unambiguous while latency is under 6 h 37 min.

**A/V sync on WebRTC** is done by the browser from RTCP sender reports, which pair each stream's RTP time with
one wall clock. str0m builds that pair from the `wallclock` given with each sample (Read: `sender_info`,
`src/streams/send.rs:841-873`). The existing driver passes the time of writing (Read:
`crates/multiview-webrtc/src/transport/whep_serve.rs:563-575`), so audio and video would be aligned on when
they were sent, which differs by the difference of their encoder delays. The cast rung passes the epoch's wall
time of the sample's `E` for both streams. It is always in the past when written, as str0m requires.

**TS contract (SRT).** libav's MPEG-TS muxer writes `PTS = packet PTS + 2 * max_delay` in 90 kHz, and
`max_delay` is 0 unless the caller sets it (Read: FFmpeg 8.0 `libavformat/mpegtsenc.c:1130-1131`,
`:1877-1900`). The engine's muxer wrapper does not set it (Read: no `max_delay` in
`crates/multiview-ffmpeg/src/mux.rs` or `crates/multiview-output/src/sink/push.rs`). So the wire PTS should be
`floor(E * 90000 / 1e6) mod 2^33`. Unverified end to end; slice 6 proves it by demuxing the engine's own SRT
output and comparing with the packets' timestamps, across a session that starts late in a run.

On the device the mapping must survive extraction. Media3's `TsExtractor` subtracts the first timestamp by
default: every public constructor without an adjuster passes `new TimestampAdjuster(0)` (Read:
`TsExtractor.java:190-243`, 1.8.0). The app constructs the extractor with its own
`TimestampAdjuster(TimestampAdjuster.MODE_NO_OFFSET)` (Read: "if timestamps should not be offset",
`TimestampAdjuster.java:80`), which still unwraps the 33-bit rollover against the previous timestamp (Read:
`adjustTsTimestamp`, `:196-212`). The renderer reports `bufferPresentationTimeUs - outputStreamOffsetUs` (Read:
`MediaCodecVideoRenderer.java:1797`, `:1840`). Unverified: that this equals the extractor's sample time for a
progressive source in every case. Slice 6 has an instrumented test that plays a stream of known PTS and
compares. The app resolves the 2^33 cycle (26 h 30 min) against the clock, as for RTP.

**Discontinuities.**

| Event | Effect on the mapping |
| --- | --- |
| Take, encoder change, feed change with the attachment kept | None. `E` continues |
| New attachment (re-attach, step-down, receiver reload) | MoQ: a new `B`. Others: nothing to reset. The measurement window restarts |
| Player reset inside an attachment (decision 3, Receiver buffers) | MoQ keeps `B`; the buffered range has a gap and the playhead moves to the new range. The window restarts |
| RTP or TS wrap | Handled by the unwrap rules above |
| Epoch step | `E` is unaffected; its wall time changes. The receiver learns it from `epoch_generation`. Before it acts on latency it makes one clock call; if the generation changed it discards its samples |
| Engine restart | Every attachment ends; the next ladder call brings a new `clock` |

**When there is no trustworthy mapping.** Each report says how the number was obtained:

| `method` | Meaning | Latency fallback |
| --- | --- | --- |
| `frame` | per presented frame, from the callbacks in the table above | allowed |
| `playhead` | `requestVideoFrameCallback` is absent: `currentTime` sampled every 100 ms (HLS and MoQ only; error up to one sample period plus one frame) | allowed (the threshold is seconds, the error is 150 ms) |
| `unavailable` | any of: `clock` is `null` or `unavailable`; WebRTC without `rtpTimestamp` in the frame metadata; SRT before the instrumented test of slice 6 passes on that app build; or the sanity check failed | **disabled** |

Sanity check: a sample is valid only if it lies between 0 and 60 s. More than 5 invalid samples in a window
set `method` to `unavailable` for the rest of the attachment. With `unavailable` the receiver reports
`latency_ms: null`, never a guessed number, and the step-down rule keeps only its two other causes: no frame
for 2 s, and transport closed.

`getSynchronizationSources()` gives an RTP timestamp with the time a frame was delivered to the track (Cited:
[WebRTC](https://www.w3.org/TR/webrtc/)), which is before decode and display. It is reported as a diagnostic
only and never drives a fallback.

**Two numbers, never mixed.** `first_frame_ms` is the time from the start of an attempt to the first presented
frame, on the receiver's monotonic clock alone; it needs no mapping and is what the attempt deadlines use.
`latency_ms` is steady state: it starts 3 s after the first frame, over windows of 5 s, and reports median,
95th percentile, sample count, `method` and the clock error.

**Tests (slices 2, 4, 5, 6).** Unit tests of the mapping with: an offset `B`; RTP values on both sides of 2^32
and TS values on both sides of 2^33; a player reset; an epoch step between two clock calls; metadata without
`rtpTimestamp`; no `requestVideoFrameCallback`; a clock error above the limit. One end-to-end test per rung
plays the latency slate and compares the reported latency with the tick number read from the picture. One
negative test per rung: a healthy stream with an unusable mapping must not step down.

#### Measurement

Two numbers, kept apart.

1. **Tick-to-present (automatic, every cast).** For each presented frame the receiver takes the frame's
   engine media time and the time it was shown, both as defined per rung in the section above, and reports
   `latency_ms` every 5 s. This number excludes the TV's own processing and is therefore not glass-to-glass.
   It is what the fallback rule uses, and only when its `method` allows it. Today's HLS report is a different
   number: Shaka's `liveLatency`, the playlist's date-time against the TV's own wall clock with no offset
   correction (Read: `receiver.js:194-201`); slice 1 replaces it.
2. **Glass-to-glass (device test, by the operator).** Slice 1 adds a latency slate: a test source that draws the
   engine's wall clock with milliseconds and the tick number in every frame. The web UI gets a reference clock
   page that shows the same engine clock, corrected by the browser's measured offset. One photograph (or a
   240 fps phone video) that contains the TV and a laptop showing the reference page gives the latency as the
   difference of the two readings. Error: one camera exposure, plus the laptop's display delay, plus the offset
   error shown on the page. This is the burned-in clock method (Reported:
   [Fora Soft](https://www.forasoft.com/learn/video-streaming/articles-streaming/latency-glass-to-glass-explained),
   [AWS](https://builder.aws.com/content/3HSKUExqs1fPmQLt7pX5Kk1Qvjp/benchmarking-glass-to-glass-latency-for-amazon-ivs-real-time-streaming)).
   Here "glass" at the sending end is the engine's tick, because the picture is generated, not filmed.

Slice 7 adds a table of both numbers per rung and device class to this ADR. Until that table exists the
ladder's order and targets are a plan, not a result.

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
- `rvfc`: `requestVideoFrameCallback` exists. Without it latency is measured from the playhead on HLS and MoQ
  and is unavailable on WebRTC (decision 2, "Engine media time on each rung").
- `srt`: on Android TV, the libsrt version the app loaded; `null` in the Web Receiver.
- `need`: what the session must carry (decision 2, Tracks).
- `skip`: rungs in the device's failure memory, with the seconds left.
- `failed`: on a later call, `{ "rung": "moq", "stage": "connect|first-frame|stall|latency", "ms": 3000 }`.

Every field is a claim by an unauthenticated device. The engine uses it only to leave rungs out. It never widens
access because of it, and it validates sizes and types strictly (unknown fields ignored, wrong types 400).

The answer:

```json
{ "v": 1,
  "clock": { "source": "epoch", "epoch_generation": 7, "media_us": 5400000000,
             "wall_unix_us": 1794705000000000, "t1_unix_us": 1794705012345000, "t2_unix_us": 1794705012345400 },
  "attach": { "rung": "moq", "url": "https://<host>/moq?...", "broadcast": "...", "protocol": "...",
              "video": { "codec": "avc1.640029", "coded_width": 1920, "coded_height": 1080, "description": "..." },
              "audio": [ { "track": "...", "codec": "mp4a.40.2", "sample_rate": 48000, "number_of_channels": 2 } ],
              "expires_in_ms": 30000 },
  "then": ["webrtc", "hls"],
  "srt": null }
```

- `clock` is defined in decision 2, "Engine media time on each rung"; it may be `null`.
- `attach` is the best rung the engine will serve this receiver now, already admitted. `then` names the rest.
  The receiver gets the next one by calling again with `failed` set; that call is also the failure report.
- For `webrtc`, `attach` is `{ "rung": "webrtc", "whep": "whep" }`: the receiver posts its SDP offer to
  `POST /cast/<token>/whep` (`application/sdp`, at most 64 KiB as `routes/whep_output.rs:30`) and gets the
  answer; `DELETE /cast/<token>/whep` ends it. The answer carries the UDP 443 host candidates and the passive
  ICE-TCP candidates on TCP 443 together (Read: [front door](../architecture/network-front-door.md) "Slice 4
  PR2"), so a network without UDP costs no extra round trip.
- For `srt`, `attach` is `{ "rung": "srt", "host": "<public host>", "port": 9010, "latency_ms": 250,
  "sid": "<22 characters>", "expires_in_ms": 30000 }`. `sid` is a one-time selector, not a secret: SRT sends
  it in clear. The app derives the passphrase from the token and `sid` (decision 5.7).
- For `hls`, `attach` is `{ "rung": "hls" }`.

What the engine leaves out, in this order: rungs the build or the run cannot serve (no MoQ provider, no SRT
listener); rungs disabled in `[cast] ladder`; rungs the caps exclude;
rungs that cannot carry `need`; rungs in `skip`; rungs removed for the receiver kind by measurement (slice 7).

The engine keeps the last probe and the playing rung per stream (bounded, like the report) and shows them to the
stream's owner and to admins in `GET /api/v1/cast/streams/{id}`: `receiver.caps`, `transport.rung`,
`transport.since`, `transport.fallbacks[]` (at most 8, each with rung, stage, time), `first_frame_ms`,
`latency_ms` (median, 95th percentile, sample count, `method`, clock error; `null` when unavailable) and
`resets` (soft, hard, last cause). The
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
requirements: what the rung then needs is WebTransport, that is Chrome 97 or later, and Media Source
Extensions. The element keeps audio and video in sync. The cost is the element's buffer; its target, its hard
limits and what happens when they are reached are in "Receiver buffers" below. Unverified: the latency this
reaches on a device; slice 4 measures it. `vdec` and
`adec` stay in the probe so that the data shows which devices could run a canvas player, if that is ever wanted.
The probe gains `mse`: `MediaSource.isTypeSupported` for the stream's codec strings.

#### Receiver buffers: hard bounds and recovery

Reviewer finding D4. A latency target is not a memory bound. Every place where the new receivers hold media
has a fixed ceiling in count, bytes and time, and a defined action when it is reached. The action always ends
at a keyframe: after anything is dropped, nothing that depends on it is given to a decoder.

**MoQ player in the Web Receiver (`moq.js`).** Media is held in three places.

| Place | Ceiling | When reached |
| --- | --- | --- |
| Groups being read from the transport | 2 per track: the one being appended and the newest | An older group's stream is cancelled, which stops its delivery. What the browser holds unread is limited by QUIC flow control, not by this script |
| Pending fragments (received, not yet appended), per track | 60 fragments, 4 MiB, 1 s between the oldest and the newest timestamp | **Soft reset** |
| Retained in each `SourceBuffer` | 2 s behind the playhead, 3 s ahead of it; 16 MiB by the player's own account | Behind: removed. Ahead above 1.5 s: the playhead jumps forward. Ahead above 3 s, or the byte account above 16 MiB: **hard reset** |

- *Appends.* One `appendBuffer` per track at a time. On each `updateend` the player appends everything pending
  for that track as one buffer (at most 4 MiB by the ceiling above). It never calls `appendBuffer` while
  `updating` is true.
- *Back buffer.* Once a second, when the buffer is idle, the player calls `remove(0, currentTime - 2)`. The
  browser may remove more than asked, up to the next random access point (Cited: coded frame removal,
  [Media Source Extensions](https://www.w3.org/TR/media-source-2/)); that is behind the playhead and harmless.
- *Byte account.* The browser does not report what a `SourceBuffer` holds. The player keeps, per track, a ring
  of at most 512 entries (timestamp, size) for fragments it appended and has not removed, and sums them. This
  is an account of bytes appended, not of the browser's memory. The browser's own limit is the second line: an
  append it cannot fit throws `QuotaExceededError` (Cited: prepare append, same specification).
- *Playhead.* Target 150 ms ahead of the playhead. Above 250 ms the playback rate is 1.03 until it is back at
  the target; above 1.5 s the player sets `currentTime` to the buffered end minus the target.

**Soft reset** (pending overflow; a gap in the frame sequence of a group; a group abandoned part-way; a frame
that arrives after a later one). The player discards the pending fragments of *both* tracks, discards every
arriving video frame until the first frame of a new group (a keyframe), and discards arriving audio frames
older than that keyframe's timestamp minus one audio frame. It then appends from there and moves the playhead
to the start of the new buffered range. Both tracks keep the base `B` of the attachment (decision 2), so audio
and video stay on one timeline and the element keeps them in sync. Cost: the picture holds its last frame for
at most one GOP.

**Hard reset** (no `updateend` or `error` within 2 s of an `appendBuffer` or `remove`; a `SourceBuffer` error
event; a media element decode error; retained ceilings exceeded; a second `QuotaExceededError`). The player
drops the `MediaSource`: it detaches the element, revokes the object URL, creates a new `MediaSource` and new
`SourceBuffer`s, appends the initialisation segments, and continues as after a soft reset. The transport
session is kept.

**`QuotaExceededError`.** The player does not retry the same append at once. It removes everything up to
`currentTime - 0.5`, waits for that `updateend`, and retries once. A second failure is a hard reset.

**Not playing.** When the element is paused or the receiver page is hidden for more than 2 s (the TV shows
another input), the player ends the attachment instead of buffering a live stream it does not show. It
attaches again when the page is visible, with a new ladder call.

**Reset budget.** More than 3 hard resets, or more than 10 soft resets, within 60 s: the receiver steps down
(stage `stall`). Every reset is counted in the report (`resets.soft`, `resets.hard`, with the last cause).

**WebRTC player in the Web Receiver (`rtc.js`).** The script holds no media: the browser's jitter buffer and
decoder do, and their limits are the browser's. The player sets `jitterBufferTarget` to 0 where the attribute
exists (Cited: `RTCRtpReceiver`, [WebRTC](https://www.w3.org/TR/webrtc/)). Unverified on a Cast device: that
it exists and what the buffer then holds. The player's own bound is on time: no frame for 2 s is the existing
step-down cause, and the same pause and hidden rule applies.

**Android TV app.**

| Place | Ceiling | When reached |
| --- | --- | --- |
| libsrt receive buffer | `SRTO_RCVBUF` set to 2 MiB (libsrt's default is 8 192 packet buffers, about 12 MiB; Cited: socket options) | libsrt discards. With too-late packet drop, on by default in live mode, late packets are skipped (Cited: `SRTO_TLPKTDROP`) |
| Between libsrt and the player | none: the `DataSource` reads from the socket into the player's own buffer when asked. There is no queue of the app's | n/a |
| Player buffer | `DefaultLoadControl`: maximum 2 s, target 16 MiB, size limit enforced over time (`setPrioritizeTimeOverSizeThresholds(false)`), no back buffer (Read: the builder's methods, `DefaultLoadControl.java:168-235`, Media3 1.8.0) | The player stops reading; the socket buffer fills and libsrt discards |

Unverified: that libsrt discards, instead of growing or blocking, when the application stops reading. Slice 6
asserts it in a test before the rung is enabled.

Recovery in the app. Loss is detected in two ways: libsrt's receive-drop counter increases, or the
`DataSource` sees a gap in the continuity counter of the video packets. Either one flushes the player. After a
flush Media3's sample queue discards samples until a keyframe (Read: `upstreamKeyframeRequired`,
`SampleQueue.java:100`, `:183`, `:623`), so no dependent frame is decoded; audio restarts with it on the same
timestamps. A live offset above 1 s is corrected by a seek to the live edge, which is the same flush. More
than 3 flushes in 60 s: the app steps down to HLS.

#### Cast media status

**Cast media status in the Web Receiver is the open risk of the MoQ and WebRTC rungs.** CAF builds the media
status that senders and Google Home show from its own player. The MoQ player feeds a `<video>` element through
a `MediaSource` of its own and the WebRTC player gives one a `MediaStream`; neither goes through CAF's player. Cited: a LOAD interceptor may return `null` so that CAF does
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
3. **Attachments are told apart by a one-time stream id, on one port.** The listener installs libsrt's listen
   callback, which receives the caller's stream id before the connection is accepted and sets that
   connection's passphrase
   (Cited: [access control](https://github.com/Haivision/srt/blob/master/docs/features/access-control.md),
   [`srt_listen_callback`](https://github.com/Haivision/srt/blob/master/docs/API/API-functions.md)).
   `multiview-srt-sys` does not bind that function today (Read: the function table,
   `crates/multiview-srt-sys/src/lib.rs:139-156`); slice 6 adds it, with `srt_setrejectreason` and `srt_getsockflag`.
4. **No SRT listener starts on libsrt older than 1.5.7.** Cited: advisory
   [GHSA-m96h-3rc9-xx6h](https://github.com/Haivision/srt/security/advisories/GHSA-m96h-3rc9-xx6h) (published
   15 Oct 2026): a handshake packet overflows a heap buffer in a listener before authentication; affected up to
   1.5.6; fixed in 1.5.7. The rule covers every listener the engine binds, not only the cast one: SRT listener
   outputs, SRT listener inputs and the cast listener. All three go through `SrtListener::bind` (Read:
   `crates/multiview-output/src/sink/push.rs:455`, `crates/multiview-cli/src/srt_ingest.rs:383`), so the check
   lives there once: it calls `srt_getversion()` on the library it loaded (Cited: the value is `0xXXYYZZ`,
   [API functions](https://github.com/Haivision/srt/blob/master/docs/API/API-functions.md)) and returns a typed
   error below `0x010507`. The listener then reports `srt-libsrt-too-old` with both versions as its
   `last_error`; the rest of the run is unaffected, as with a library that fails to load. Caller-mode SRT is
   not covered by the advisory and is not changed. This is slice S (decision 8); it does not wait for the ladder.

   State of the test host. At 00:00 UTC on 15 Nov the engine on mda001 had Debian's libsrt 1.5.4-1+deb13u1
   mapped, with a public listener on UDP 9000 (Measured). The lead then built upstream 1.5.7 on the host. At
   01:02 UTC the running engine maps `/opt/libsrt-1.5.7/lib/libsrt.so.1.5.7` and UDP 9000 answers an SRT
   induction on IPv4 and IPv6 (Measured).

   Two costs of the rule, stated plainly. First, the shipped container images install `libsrt1.5-openssl` from
   Debian trixie, which is 1.5.4 (Read: `deploy/Dockerfile:111`, `:208`, `deploy/Dockerfile.nvidia:108`,
   `:207`; the version from Debian's tracker). The CI jobs install the same package name on `ubuntu-latest`
   (Read: `.github/workflows/ci.yml:747`, `:841`, `:965`); its version there was not checked and is assumed to
   be below 1.5.7. With the check in place every SRT listener in those images refuses to start, and the CI
   legs that run a real listener fail, until the images and CI carry 1.5.7. Slice S therefore changes both in
   the same pull request. Second, a distribution that
   backports the fix without changing the version number is refused too. There is no override: a switch that
   re-opens a pre-authentication memory fault is not offered. Whether Debian's 1.5.4-1+deb13u1 is in fact
   affected was not tested and no longer matters to this rule.
5. **Latency setting.** 250 ms receive latency on both ends at first. Cited: "The recommended value of
   TsbpdDelay is 3-4 times RTT", minimum 120 ms
   ([draft-sharabayko-srt-01](https://www.ietf.org/archive/id/draft-sharabayko-srt-01.txt), section 4.5.1). 250 ms
   covers a 60 ms round trip four times. The app reports the measured round trip; slice 7 sets the final value
   from device data.
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
8. **Flood floor for the SRT port.** The first handshake step only sets a cookie, "so that it doesn't allocate
   resources" on the listener (Cited: the same draft, section 4.3.1.1). The runbook adds a per-source nftables limit for the port, as the UDP 443 floor
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
3. **Requested feed change (as PR 409).** A change covers any component: the picture, an audio slot, the
   audio routing, a layout's definition, a subtitle patch. A token whose owner may read the whole new closure
   keeps its attachment and the content changes in place. A token whose owner may not is revoked first and its
   attachment is fenced before the first packet of the new content is admitted. Item 4 is what makes "before
   the first packet" true in the data path.
4. **Unrequested changes: every media component carries its authorization identity, and the default is deny.**
   Reviewer finding D2. The engine can change what an output carries by itself (a fallback), and a cast carries
   more than a picture: each audio slot is routed on its own, and a bus name does not say what is behind it.
   For HLS the media route compares the engine's readback on each request. A push rung has no request, so the
   rule moves into the data path and covers every component.

   **Components.** Video; each audio slot the rung carries; each subtitle slot, on a rung that carries
   subtitles (none of the push rungs does in the first version, HLS does).

   **Identity.** For each component of each output the engine holds a 64-bit *content identity*. It changes
   whenever anything that the authorization closure of that component depends on changes: the bus or source the
   component is taken from; the layout shown and the revision of its definition (cells, sources, layout audio);
   the audio choice, the track and the input behind it, and the revision of the audio routing; the subtitle
   patch. These are the inputs of the closure the token check computes today (Read: `video_needs`,
   `audio_needs`, `cast_streams/reauth.rs:333-393`; lane cast2 adds `subtitle_needs` and the audio slots).
   A change inside a bus that the closure treats as one grant does not change the identity: Program, Preview
   and Clean need the bus grant only (Read: `Need::Bus`, `reauth.rs:339`), so a cut on the switcher is not a
   change here.

   **Captured with the data, not read afterwards.** An encoder emits a packet later than the frame went in, so
   reading the routing state when the packet comes out can label old content with a new identity or the
   reverse. Output timestamps are a function of the tick, so the identity is too: per output and component the
   engine keeps a short change list of `(first tick, identity)`, written by the code that applies the change at
   that tick. A packet takes the identity of the last entry at or before the tick of its own timestamp. The
   routing readback already records the tick a feed took effect (Read: `since_tick`,
   `crates/multiview-control/src/output_routing.rs:379`). The list is bounded (16 entries); a packet whose
   timestamp cannot be resolved has no identity.

   **Who mints an identity.** The control plane mints one for each change it requests, after it has computed
   the new closure, and passes it with the request. For a change it did not request, the engine mints one from
   a range the control plane never uses. Nobody is cleared for an engine-minted identity until the control
   plane has seen it and authorized against the state it stands for.

   **Clearance.** Each attachment holds the set of identities its token is cleared for, per component: an
   immutable list of at most 9 values (the current one and the unsettled changes; the registry already bounds
   those at 8, Read: `MAX_CARRY`, `cast_streams/mod.rs:68`), replaced as a whole by the registry and read
   without a lock. Clearance for a requested change is added only for tokens whose owner passes the check for
   the new closure, inside the critical section that re-authorizes tokens today and before the request goes to
   the engine. The old identity leaves the list when the engine reports the outcome.

   **The gate.** At the last point where media is still common to several viewers and the first where it
   belongs to one: the registry's offer for a MoQ lease, the distributor's hand-off to an attachment's queue
   for SRT, the driver's write to a viewer's session for WebRTC (decision 6). There, before anything is
   enqueued or written for that attachment, every component in the window must have an identity that is in
   the attachment's list for that component. Identities therefore travel with the packets through the SRT
   distributor and the WebRTC egress feed. One miss, or a component without an
   identity, and the whole window is refused, the attachment is fenced and nothing later is offered to it. The
   check is a bounded comparison of a few integers, of the same kind as the revocation flag and generation the
   registry tests today. It takes no lock and does not allocate.

   **Packets already buffered.** A fence stops enqueue. The attachment's own queue is then discarded, not
   drained: nothing more is handed to the transport, and the transport is closed abruptly (QUIC session close,
   `srt_close`, DTLS close) instead of flushed. What the transport had already accepted is at most its own
   bounded buffer (decision 6) and consists only of windows that passed the gate, that is, content the owner
   was cleared for when it was produced. No packet of the refused identity was ever enqueued.

   **Re-attachment.** The receiver sees the transport end with reason `feed-changed` and makes a ladder call.
   That call runs the shared token check, which first re-authorizes the token against the engine's present
   state (the existing barrier, `reauth::on_media`). If the token survives, the new attachment is cleared for
   the identities current at that moment. If not, the call answers 404 and the cast ends, as for HLS. Three
   `feed-changed` ends inside 10 s make the receiver play HLS for the rest of the session, where every request
   is checked.

   **What is confirmed and what is not.** The contract above is fixed. Unverified: where the dispatcher can
   attach the change list to video and audio packets, and whether every unrequested change in the engine goes
   through code that can mint an identity. Slice 2's design note settles the mechanism and may replace it, but
   not the contract, and slice 2 does not merge without these tests, each run against every push rung (against
   a fake push attachment in slice 2, then the real rung in slices 4, 5 and 6), each failing on revert:

   | Case | Must hold |
   | --- | --- |
   | Picture fallback to a feed the owner may not read | zero packets of the new feed delivered; attachment fenced |
   | Same picture, an audio slot moves to a source the owner may not read (unrequested, and by the audio-routing route) | zero audio packets of the new source delivered; the video of that window is refused with it |
   | Same bus name, new dependency: a layout is edited to include a source the owner may not read, the output stays on that layout | zero packets produced after the edit's tick delivered |
   | The change lands between production and enqueue | a window produced before the change tick is delivered or dropped, never relabelled; a window produced after it is refused |
   | A packet without a resolvable identity (change list overflow, injected) | refused; attachment fenced |
   | Fence with a full attachment queue | queue length is 0 after the fence; the transport receives nothing after it |
   | Owner cleared for the new feed | the attachment survives the change with no gap beyond the encoder's own |
   | Re-attachment after a fence, owner cleared for the present state | new attachment admitted; cleared only for the present identities |

   The 5 s sweep of ADR-0111 (re-scoped keys, re-pointed audio tracks) stays as a second line. No push rung
   relies on it.
5. **Token lifetime.** Today a live token expires 60 s after its last playlist fetch (Read:
   `cast_streams/registry.rs:48-50`). A push rung fetches no playlist. A successful report or ladder call from
   a stream that has a live attachment counts as that fetch. A device that vanishes stops reporting; its
   transport times out (SRT peer idle 5 s, QUIC idle, ICE consent) and the token expires 60 s later, as today.
6. **Per-object authorization.** No new route takes an object id from the caller. The token names one stream,
   which names one output. The SRT selector and the MoQ broadcast name are looked up, never trusted as an
   address: a selector that matches no pending attachment is refused before any key work (item 7). The new fields of
   `GET /api/v1/cast/streams/{id}` follow the route's existing rule (owner or admin). MoQ and WebRTC admission
   run with the token owner's current principal (Read: `resolve_owner`, `reauth.rs:282`), so the providers'
   own dependency checks see the owner's present grants.
7. **SRT: a one-time selector in clear, a passphrase from the token, and no state an unauthenticated peer can
   change.** Reviewer finding D3. SRT sends the stream id in clear before any key exchange; that cannot be
   avoided. So what travels there must identify nothing durable, and a failed handshake must cost its sender
   only.

   **Selector.** Each ladder answer for the SRT rung carries a fresh selector `S`: 16 bytes from the OS random
   source, base64url (22 characters). It is not derived from the token. It names one pending attachment, lives
   30 s, and is consumed by the first authenticated connection that uses it. A token has at most one unconsumed
   selector; a new ladder call replaces it. Two attachments of the same cast show unrelated selectors, so an
   observer cannot link them to each other or to a token, and a selector seen on the wire is useless once the
   connection it belonged to is up. A reconnect after a dropped connection therefore needs a new ladder call,
   which costs one HTTPS round trip. The stream id is `#!::r=cast/<S>,m=request`.

   **Passphrase.** With `T` the 32 token bytes:

   ```
   prk        = HKDF-SHA256-Extract(salt = "conspect-cast-srt-v2", ikm = T)
   passphrase = base64url(HKDF-Expand(prk, "passphrase" || S, 32))      43 characters
   ```

   - 256 bits of entropy, inside SRT's 10 to 80 characters (Cited: `SRTO_PASSPHRASE`,
     [socket options](https://github.com/Haivision/srt/blob/master/docs/API/API-socket-options.md)). Key length
     32 bytes (AES-256), counter mode. GCM is a preview build option in libsrt (Cited, same page) and is not used.
   - Each attachment has its own key. Knowing `S` gives nothing without `T`.
   - The app computes it from the token it holds and the `S` it was given. The engine computes it when it
     issues `S` (it can derive the token from its stored seed and process pepper, Read:
     `cast_streams/token.rs:85-98`) and keeps it with the pending entry, which is erased on use or expiry and
     whose `Debug` prints nothing.

   **The listen callback, in this order.** It runs for a caller that has returned the listener's cookie; the
   cookie is "crafted based on host, port and current time" (Cited: draft-sharabayko-srt-01, section 4.3.1.1),
   so the source address is one that receives the listener's replies.

   1. Budget check for the source and for the listener (below). Over budget: reject. Nothing else is touched.
   2. Look up `S` among the pending entries (at most 16, compared in constant time over all of them). Unknown,
      expired or consumed: reject. No key derivation happens for an unknown selector.
   3. Set the entry's passphrase and key length on the new socket and return. libsrt then does the key exchange
      and rejects a caller whose passphrase differs (Cited:
      [access control](https://github.com/Haivision/srt/blob/master/docs/features/access-control.md)). The
      callback cannot compare passphrases and does not try.

   Every rejection by the callback uses one code, so the answer does not say which step refused. The callback
   never writes to a pending entry, a token or an attachment. A failed handshake therefore changes two
   counters, the source's budget and the listener's, and nothing else. There is no count of failures per
   selector or per token, and no refusal period for either: the rule of the first draft (four failures lock the
   id for a minute) is withdrawn.

   **Attach, after authentication only.** `srt_accept` returns a socket only when libsrt has completed the
   handshake. With enforced encryption, which is the default and is set explicitly, a connection whose key
   state is not secured on both sides is rejected (Cited: `SRTO_ENFORCEDENCRYPTION`, socket options). The
   accept thread still reads `SRTO_KMSTATE` and `SRTO_RCVKMSTATE` and requires `SRT_KM_S_SECURED`. Then one
   call into the token registry does all of this in one critical section, or none of it: the entry for `S`
   exists, is unexpired and unconsumed; its token is live and armed at the output's current authorization
   generation; the entry is marked consumed; the new attachment is registered with its fence and its cleared
   identities (item 4); and an older SRT attachment of the same token, if any, is fenced. On any failure the
   new socket is closed and the older attachment is left exactly as it was. An attachment that is playing is
   thus replaced only by a caller that proved it holds the token, at the current generation.

   **Budgets before authentication.**

   | Budget | Size | Over budget |
   | --- | --- | --- |
   | Per source: one IPv4 address, or one IPv6 /64 | burst 32, refill 4 per second | callback rejects that source's handshakes |
   | Source table | 1 024 sources; the least recently seen is evicted | an evicted source starts with a full budget; the listener budget still applies |
   | Listener, all sources | burst 128, refill 32 per second | callback rejects every new handshake |
   | Accepted, not yet attached | the listen backlog, 16 | libsrt rejects (`SRT_REJ_BACKLOG`) |
   | Pending selectors | 16 (the SRT connection cap, item 9) | the ladder does not offer SRT |

   Before the cookie is returned the listener keeps no state per caller; the nftables floor of decision 4
   limits packets per source there.

   **What the budgets do not protect, stated.**

   - *Shared address.* Devices behind one NAT address share one source budget. It is sized so that legitimate
     use cannot reach it: the whole engine has at most 32 live cast tokens (Read: `MAX_STREAMS`,
     `cast_streams/mod.rs:46`), each needing one handshake per attachment. But a hostile host behind the same
     address as a viewer can spend that budget, and new SRT attachments from that address then fail.
   - *Many real addresses.* Eight sources at their full rate use up the listener budget; an IPv6 /48 holds
     65 536 such sources. New SRT attachments from anyone then fail.
   - *On the path.* An attacker who can read the viewer's packets can also drop them.

   In all three cases the damage is bounded the same way: an attachment that is already playing is not
   affected, because a playing attachment needs no handshake and no failed handshake can reach it; and a
   viewer whose new attachment is refused plays HLS over TCP 443 (decision 2), which this listener cannot
   influence. The worst outcome of an attack on the SRT port is "no SRT for new casts", never "no picture".

   **Tests (slice 6), each failing on revert.**

   | Case | Must hold |
   | --- | --- |
   | A valid receiver is playing; a second host sends 1 000 handshakes with that attachment's selector and wrong passphrases | the attachment keeps playing with no gap; its counters show no reconnect |
   | The same flood runs while a valid receiver makes its first connection from another address | the valid receiver attaches within its deadline |
   | A valid caller reconnects with a fresh selector while its old attachment still exists | the old one is fenced only after the new one attached; a failed new one leaves the old one playing |
   | A selector is used twice | the second use is rejected by the callback |
   | Unknown selector; expired selector; right selector with a wrong passphrase; right selector after the token was revoked | no media; the three callback rejections carry the same code |
   | One source exceeds its budget | that source is rejected; another source attaches |
   | Selectors of two attachments of one token | no common substring beyond chance; neither equals any HKDF output of the token |
8. **No secret on argv or in logs.** The passphrase goes to libsrt through the socket option
   (`ListenerOptions::with_encryption`, Read: `multiview-srt-sys/src/lib.rs:350`), never through a `srt://` URL,
   a libav option string or a process argument, on the engine and in the app. The MoQ lease URL is marked "never
   log" (Read: `moq.rs:77`) and stays so. The ladder answer is `no-store`. Logs carry a token's first 6
   characters at most, as `receiver.js` does today. The engine's SRT link status shows the peer address and
   counters, never the selector.
9. **Bounds.** Ladder calls: 1 per second per token, burst 4, body 2 KiB. Attachments: one per token per rung;
   at most 32 tokens in all (Read: `MAX_STREAMS`, `cast_streams/mod.rs:46`); at most 16 connections on the
   cast SRT listener, as an SRT listener output has (Read: `MAX_LISTENER_CLIENTS`,
   `crates/multiview-output/src/sink/push.rs:85`); MoQ attachments limited by the run's 16 registry
   subscriptions, shared with the desk (decision 6). WHEP offer 64 KiB. Stored probe, reset counters and
   fallback history: fixed size per stream. Media buffers: decision 6.
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
subscribe to the cast output's entry the same way. Limits already in place: a
two-window handoff per subscriber, and a subscriber that falls behind loses its whole window and resumes at the
next verified keyframe (Read: `output_window_registry.rs:43`, module doc).

The registry admits 16 subscriptions, and that number is for the whole run, not per output: one counter belongs
to the one registry that serves every output (Read: `MAX_PEERS`, `try_reserve_slot`, `slots`,
`output_window_registry.rs:38`, `:376-388`, `:797`). The desk's MoQ viewers use the same 16. The cast rungs
must not use them up, so they subscribe like this:

| Rung | Registry subscriptions | Fan-out to attachments |
| --- | --- | --- |
| MoQ | one per lease, as on `main` | none: the lease is the attachment |
| SRT | one per cast output that has an SRT attachment | a distributor with one bounded queue and one writer thread per attachment, as the SRT listener output has |
| WebRTC | one per cast output that has a WebRTC attachment | the egress feed and its driver, as the `webrtc` output has |

A run with two cast outputs therefore uses at most four subscriptions for SRT and WebRTC, however many viewers
they have. MoQ viewers of cast and of the desk compete for what is left; when none is left the ladder does not
offer MoQ and the receiver gets the next rung. Raising the cap is not part of this ADR. Two things to settle in
slice 2, both Unverified here: that a `cast` output's binding enters that registry (nothing in the registry
names the kind; `binding_ids` covers every configured output, `pipeline.rs:5135-5139`), and that the encoder's
no-reorder proof holds for the production encoder on mda001 (a subscription is refused without it, Read:
`output_window_registry.rs:581`).

**Activation.** A cast output is gated: it packages nothing until a stream activates it. The first ladder
attachment activates the output exactly as the first HLS playlist request does today, and the last one ending
starts the same 10 s linger. A push rung does not use the 20 s pre-roll ring; it starts at the next keyframe.

**Queues (invariant #10).** Every hand-off toward a viewer has a ceiling in count, in bytes and in age, and
drops when any one is reached (reviewer finding D4). The packets in these queues are reference-counted views of
the encoder's output, so a byte ceiling is what limits how much encoder output a slow viewer keeps alive.

| Hand-off | Count | Bytes | Age | When reached |
| --- | --- | --- | --- | --- |
| Dispatcher to a rung's subscription | 2 windows in hand-off; the binding queue behind it 512 packets | 16 MiB | 2 s | whole window dropped, discontinuity, resume at a verified keyframe (Read: `MAX_QUEUE_PACKETS`, `MAX_QUEUE_BYTES`, `MAX_QUEUE_AGE`, `crates/multiview-output/src/batch.rs:30-36`; `WINDOW_HANDOFF`, `output_window_registry.rs:43`) |
| MoQ subscription | 16 sessions | 8 MiB per subscription, 64 MiB in total (Read: `crates/multiview-moq/src/provider.rs:454-455`, `:867`) | 2 s per track (Read: `with_max_age`, `crates/multiview-moq/src/lib.rs:100`) | window refused; the lease survives or ends by class of error |
| SRT attachment queue | 512 packets | **8 MiB, new** | **2 s, new** | everything queued is discarded, then the session is re-gated to the next keyframe |
| SRT library send buffer, per connection | set by `SRTO_SNDBUF` | **2 MiB, set explicitly** (libsrt's default is 8 192 packet buffers, about 12 MiB) | the negotiated latency: libsrt drops packets too late to deliver (Cited: `SRTO_TLPKTDROP`, socket options) | libsrt drops; a send that cannot complete ends at the existing 5 s write deadline and the attachment closes |
| WebRTC egress feed, per cast output | 512 samples | **8 MiB, new** | **2 s, new** | oldest dropped, then video is held back until the next keyframe |
| WebRTC library send buffers, per viewer | str0m's defaults, set explicitly: 1 000 video packets, 50 audio packets (Read: str0m 0.16.2 `src/config.rs:572-573`) | about 1.2 MiB at one MTU each | n/a | the library's own eviction |
| Opus tap, per cast output | **8 blocks** | fixed by the count: a block is one tick of 48 kHz stereo float audio, about 13 KiB at 29.97 frames per second | 8 ticks | oldest dropped |
| Encoded Opus toward the feed | counted inside the egress feed above | | | |

What is new and why. The SRT listener output on `main` bounds its per-caller queue by packets only (Read:
`CLIENT_QUEUE_CAP`, `crates/multiview-output/src/sink/push.rs:69`). A packet is one access unit, and a keyframe
at 1080p can be several hundred kilobytes, so 512 of them is not a memory bound. The cast attachment adds a
byte and an age ceiling and discards the whole queue when one is reached, because dropping from the middle of a
GOP would hand the muxer frames whose reference is gone. The WebRTC egress feed has the same gap (Read:
`MAX_EGRESS`, `crates/multiview-webrtc/src/egress.rs:36`, a count only) and gets the same two ceilings. Both
changes also improve the existing outputs that use those types; they ship in slices 5 and 6 behind tests. The
Opus tap follows the preview tap's size (Read: `PROGRAM_AUDIO_RING`, `crates/multiview-cli/src/preview.rs:71`).

Totals at the caps (16 SRT connections, 16 MoQ sessions, 32 WebRTC viewers on at most 32 tokens): SRT
16 x (8 + 2) MiB = 160 MiB; MoQ 64 MiB; WebRTC 8 MiB per cast output plus 32 x 1.2 MiB. These are ceilings of
referenced encoder output, reached only when every viewer is stalled at once.

No hand-off waits for a viewer. Nothing a device sends (probe, report, SDP, SRT handshake) reaches a data-plane
thread: control routes answer from the registry, and the SRT listen callback reads a bounded table.

**Long-run and stalled-consumer proof.** A short successful playback proves none of the above. Each push rung
has these tests, asserting counts and bytes against the ceilings, never percentiles:

| Test | Runs | Asserts |
| --- | --- | --- |
| Receiver soak (MoQ, slice 4) | 30 minutes in the harness with the real CAF SDK, taking a heap sample and the buffered ranges every 10 s | retained range never above 5 s; the player's byte account never above 16 MiB; JS heap after the first minute never more than 32 MiB above its value at one minute; zero resets on a clean link |
| `updateend` withheld (MoQ) | the harness wraps `SourceBuffer` so that an append never completes | pending never above 60 fragments, 4 MiB, 1 s; a hard reset within 2.5 s; picture again within one GOP after it |
| Decoder slower than real time (MoQ) | the harness throttles the page's CPU | soft resets, each resuming at a keyframe; no frame appended whose group lost an earlier frame; step-down after the reset budget |
| `QuotaExceededError` injected (MoQ) | on the next append, then twice in a row | one removal and one retry; then a hard reset |
| A/V after recovery (every rung) | the latency slate with a tone burst on a known tick, before and after a forced reset or flush | the audio burst and its picture are within 80 ms of each other after recovery |
| Engine: viewer stops reading (every push rung) | a real client that stops reading for 60 s, then resumes | each ceiling in the table above holds throughout; the process's resident memory returns to within 16 MiB of its starting value; the clock loses zero ticks and every other binding zero packets, with a negative control that must fail |
| Engine: many stalled viewers | every attachment slot filled with stalled clients | the totals above hold; admission refuses the next attachment |
| App: reader stalls (SRT, slice 6) | instrumented test that stops the `DataSource` from reading for 10 s | libsrt's buffer stays at its 2 MiB setting; after resuming, a flush and a keyframe start; no growth of the Java heap beyond the player's 16 MiB target |

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
| Minimum Android | the highest floor among the three libraries. `srtdroid` 1.10.1 needs API 21 (Read: `AndroidVersions.MIN_SDK` at that tag); the floors of Media3 and the Cast TV library were not checked (slice 6) |
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

### 8. Slices

Order matters: the first three slices decide whether the later ones are built as designed.

| # | Pull request | Class | Depends on | What it proves |
| --- | --- | --- | --- | --- |
| 0 | This ADR and its line in the index. The cast runbook is not touched: it describes what runs, and nothing here runs yet | R0 | none | The design is reviewed before code |
| S | **libsrt floor for every SRT listener.** *Owned by ship lane `srtfloor`, from `main`; not a task of this team.* `srt_getversion` in `multiview-srt-sys`; `SrtListener::bind` refuses below 1.5.7 with a typed error; libsrt 1.5.7 in the container images and the CI jobs; runbook line | R2 | none (touches no cast file; does not wait for PR 466) | A listener on an older library does not bind and says why; the existing SRT listener tests pass on 1.5.7; the image's SRT smoke test passes |
| 1 | **Latency slate and evidence.** A test source that draws the engine clock (ms) and the tick; the reference clock page; tick-to-packet delay of the shared encoder as a metric; the receiver's HLS reports gain tick-to-present | R2 | PR 466 merged | The baseline: glass-to-glass and tick-to-present for today's HLS on the operator's Chromecast, and the encoder delay that bounds every rung. Records the device's user agent |
| 2 | **Ladder endpoint and one authorization.** `POST /cast/<token>/ladder` with the probe stored and shown; the shared token check; attachment fences set on revoke, expiry and feed change; liveness by report; `[cast] ladder`. The answer offers `hls` only. `receiver.js` sends the probe and still plays HLS | R3 | 1 | One token governs every transport: the PR 409 abuse tests pass against a fake push attachment, and so does every case of the table in decision 5.4 (picture fallback, same picture with a new forbidden audio source, same bus with a new dependency, the production-to-enqueue race, a packet without identity). The clock exchange and its error bound are tested. From the first device cast: the real capability table of that device |
| 3 | **CAF spike, in the harness, no product code path.** A `<video>` element fed from outside CAF's player; LOAD interceptor returning `null`; `setMediaElement`, `setMediaInformation`, `broadcastStatus` | R1 (tools only) | 2 | Whether a Web Receiver can show a picture CAF did not load and still give senders a correct media session. Checks the page policy against a cross-origin WebTransport session. A fail ends slices 4 and 5 |
| 4 | **MoQ rung.** Engine: cast outputs admitted to MoQ leases through the ladder; lease renewal by the ladder call. Receiver: `moq.js` (WebTransport, CMAF wrap, `SourceBuffer`), deadlines, step-down, memory | R3 | 3 passed; the probe data shows a device with `wt` and `mse` | MoQ plays through the real CAF SDK in the harness with media status correct; the receiver soak and the stalled-append, slow-decoder and quota tests of decision 6 pass; reported latency matches the slate's tick (decision 2); the authorization cases of decision 5.4 pass on the real rung; a wedged or stalled MoQ attachment costs the clock zero ticks and other bindings zero packets; device test with glass-to-glass |
| 5 | **WebRTC rung.** `POST` and `DELETE /cast/<token>/whep`; one Opus encoder per cast output; PLI ignored; receiver `rtc.js`; ICE-TCP | R3 | 3 passed | WebRTC plays in the harness with UDP blocked (TCP 443 only); wire RTP time equals engine time across the 2^32 wrap and latency is `unavailable`, with no step-down, when the frame metadata lacks it (decision 2); the Opus encoder count stays one with several viewers; the egress byte and age ceilings and the authorization cases of decision 5.4 hold; chaos as slice 4; device test |
| 6 | **SRT rung and the Android TV app.** Engine: listen callback in `multiview-srt-sys`, the cast SRT listener, one-time selectors and the HKDF passphrase, pre-authentication budgets, attachment per token. App: `android/cast-tv/`, CI job that builds the APK. Senders: `androidReceiverCompatible`, `supportedAppTypes`, behind `[cast] android_tv` | R3 | 2, S | Engine: a libsrt caller with the issued selector and the derived passphrase receives valid MPEG-TS; the test table of decision 5.7 passes (a flood of wrong-passphrase handshakes on a known selector neither evicts nor locks out the valid receiver; unknown, expired and reused selectors and a revoked token get nothing); wire PTS equals engine time (decision 2); churn and a wedged caller do not slow the producer. Device: the app launches when installed, the Web Receiver when not; SRT plays; a blocked port falls to HLS and the next cast to the Web Receiver |
| 7 | **Measured ladder.** Run the matrix; set the offer order, deadlines, SRT latency and player buffers from the data; remove any rung that does not beat the one below it on a class; enable `android_tv` by default if slice 6's first device step passed; runbook; the measured table added to this ADR | R2 | 4, 5, 6 as built | The ladder's order and targets are measurements. Every cell of the table is filled or marked as not built, with the reason |

Slice S is a security fix for every shipped image, so it is done by ship lane `srtfloor` and not by this team;
slice 6 still depends on it. Slices 4, 5 and 6 are independent of each other
after 2 (and 3 for the web rungs). Slice 6 is the largest and
has the longest operator lead time, so it starts as soon as 2 is merged. If the probe data from slice 2 shows
that no device in hand has `wt`, slice 4 is deferred, not built blind.

**Lane cast2 (#418, PR 466).** It changes the same files slices 1, 2, 4 and 5 touch: `receiver.js` (263 lines:
track helpers, `onEditTracks`, `playbackInfo`, a 2000-byte report cap), `cast_streams/` (the gate's history and
the run-bound view) and `routes/cast_public.rs`. Rule: no slice here opens a pull request against those files
until PR 466 is merged; each then starts from that `main`. New receiver code goes into new files (`ladder.js`,
`moq.js`, `rtc.js`); `receiver.js` gets one call at the end of `onLoad` and one in the error path, nothing
moved or reformatted. The report's `tracks` field and the 2 KiB cap are kept; ladder data travels on the ladder
route, not in the report. Every receiver change ships to the hosted copy with the new sha256, as ADR-0111 says.

**Tests and proof, by slice.** Slice 2: route tests beside `cast_streams_routes`, each failing on revert.
Slices 4 to 6: pipeline chaos tests beside `cast_output/pipeline_tests.rs`, asserting counts and bounds, never
tight percentiles; `tools/cast-proof/run.sh` gains one check per rung. The fake Cast platform cannot show device
decode, device Chrome, or a TV's delay: every rung needs the device test before its slice is called done.

### 9. Operator-only steps

Nothing here is a decision. Each is an action only the operator's accounts or hands can perform. They go to the
captain as one list; none blocks slices 0 to 5 from merging.

| # | Step | Needed for | When |
| --- | --- | --- | --- |
| 1 | Cast SDK Developer Console, application `8E05190C`: add the Android TV package `studio.conspect.cast.tv` | slice 6 device test | before the first APK is installed |
| 2 | Same Console: register the Cast serial number of each Google TV or Android TV test device (the software serial shown in the device's Cast settings, not the one on the box), then reboot the device | slice 6: a sideloaded app is launched only on a registered device | with step 1 |
| 3 | On the test device: enable developer options and USB or network debugging; install the APK from the CI artifact with `adb install` | slice 6 | when slice 6's pull request has a green build |
| 4 | Estate firewall: confirm UDP 9010 reaches mda001 from the test TV's network (it is inside the existing 9000 to 9100 SRT grant; no new rule is expected). If the TV is on a network other than the office's, name it | slice 6 | before the device test |
| 5 | Baseline device test: cast the latency slate to the Chromecast over HLS; one photograph or slow-motion clip showing the TV and the reference clock page | slice 1 | after slice 1 is deployed |
| 6 | Device tests per rung: the same photograph for MoQ and WebRTC on the Chromecast, and for SRT on the Android TV device, plus "app not installed" and "SRT port blocked" on the Android TV device | slices 4, 5, 6, 7 | as each is deployed |
| 7 | Deploy the receiver files to `cast.conspect.studio` (the Worker in `conspect-studio/web`) at each receiver change. The lead did this for PR 439; listed here because it uses the operator's Cloudflare account | slices 2, 4, 5 | at each merge |
| 8 | Release signing key for the APK: create it, store it in 1Password, add it as a CI secret | a release-signed APK; not needed for debug builds on registered devices | before any device outside the test set |
| 9 | Google Play listing and publishing the Cast application, if casting to unregistered Android TV devices is wanted | general availability of the SRT rung | after slice 7 |
| 10 | Licence notices for the app (libsrt MPL-2.0 source offer, Apache-2.0 notices): legal text is not written as engineering work | distributing the APK beyond test devices | before step 9 |

## Rationale

- **Why a ladder decided by the receiver's report, not by the sender.** The sender cannot see what the device's
  browser supports or what its network passes. The receiver can test both. Keeping the LOAD unchanged also
  keeps every existing sender, including the engine-side one, working without a release in step.
- **Why SRT only in a native app.** It is the only place a UDP socket exists. It also has the best decoder
  path: `MediaCodec` to a video surface, with no browser between the stream and the screen.
- **Why MoQ before WebRTC in the Web Receiver.** Same picture bytes as every other output, AAC kept, every
  audio track kept, no per-viewer encoder state, and the product already runs it for the desk. WebRTC brings an
  Opus encode and per-peer ICE state; its strength is reach (TCP 443), which is why it sits below.
- **Why HLS stays the floor and stays untouched.** It is the only rung proven on a real device (14 Nov 2026).
  Every other rung may be absent on a given device or network, and the cast must still play.
- **Why no automatic step up.** With one decoder a step up is a visible glitch on a picture that was playing.
  A step down fixes something the viewer already sees as broken; a step up does not.
- **Why one port, a one-time selector and a derived passphrase.** One firewall rule covers any number of casts.
  The only secret is the token already in the LOAD: the passphrase is computed from it at both ends and is
  never sent. The selector that SRT must show in clear is random and used once, so it identifies no token and
  gives an observer nothing to aim at later.

## Alternatives considered

| Alternative | Rejected because |
| --- | --- |
| LL-HLS parts on the existing stream | ADR-0111 found the Cast receiver runs Shaka with low-latency mode off. Even working, it lands at 2 to 5 s (Reported), above every target here |
| WebRTC first in the Web Receiver, no MoQ | Adds an Opus encode for every cast and drops multi-track audio, where MoQ carries the shared encode unchanged. Kept as the second rung |
| MoQ over WebSocket when WebTransport is absent | TCP head-of-line blocking; the MoQ design note already excludes that fallback for the desk; it would be a third web transport to test on devices |
| MoQ, WebRTC and SRT players all in the Android TV app | Three media stacks in one app. The launch checker reaches the Web Receiver's rungs for free |
| A new Cast application id for the low-latency receiver | The brief fixes `8E05190C`; Cast Connect is designed for one id with two receivers |
| The sender picks the rung and sends it in `customData` | The sender does not know the device's capabilities; the engine-side sender and Google Home would need the same logic |
| A second, zero-delay encode for cast | An encoder session per cast output. Decided only by data: if slice 1 shows the shared encoder's delay is a large part of the budget, a tuned encode becomes its own decision |
| A per-stream SRT passphrase sent in the LOAD or the ladder answer | A second secret in transit and in device logs. Derivation needs none |
| An SRT stream id derived from the token (the first draft) | It is the same for every connection of a cast, so an observer can recognise a cast and aim handshakes at it. A random one-time selector costs one field in the ladder answer |
| Locking a selector or token after failed handshakes | Lets anyone who saw the selector deny the real viewer. Budgets are per source and per listener only |
| One SRT port per cast stream | A firewall rule per stream; the listen callback gives per-stream keys on one port |
| Canvas rendering for MoQ on the device (the desk player as it is) | The receiver page is a 720p graphics plane on a Chromecast; see decision 3 |

## Consequences

- **Invariants #1 and #10.** Stated in decision 6. Slices 2, 4, 5 and 6 are R3, each with a design note and
  chaos tests.
- **Invariant #7.** No video encoder is added. One Opus encoder per cast output exists while it has WebRTC
  viewers.
- **New surface.** Three token routes (`ladder`, `whep` POST and DELETE), one UDP listener (off by default),
  two receiver script files, one Android app, one CI job with the Android toolchain. No Android toolchain
  exists in the build environment today (Measured: no `java`, `gradle` or Android SDK on the build host).
- **Default build.** Unchanged: `cargo check --workspace` gains no native dependency. The SRT rung needs the
  existing opt-in `srt-libsrt` feature and libsrt 1.5.7 or later on the host.
- **Operations.** Every host that runs an SRT listener needs libsrt 1.5.7 or later once slice S is deployed
  (Debian trixie ships 1.5.4; 1.5.7 is in forky and sid, Read: Debian's security tracker for `srt`). mda001
  already loads 1.5.7 from `/opt/libsrt-1.5.7`. Receiver changes still ship twice, engine copy and hosted copy.
- **What a viewer sees when things go wrong.** A slower start by at most the sum of the deadlines of the rungs
  that failed, once per 15 minutes; then a picture. Never a blank screen that HLS would have filled.
- **Honest limit.** Until slice 7, every latency figure in this document is a target. If the measured ladder
  shows no rung under 2 s on class A devices, the result for those devices is "HLS, 6 to 10 s", and this ADR
  will say so.

## Open questions, and the slice that closes each

| Question | Closed by |
| --- | --- |
| Chrome version, `WebTransport`, WebCodecs, `RTCPeerConnection` and MSE support on the operator's Chromecast and on a Google TV device's Web Receiver | slice 2 (probe stored on the first cast) |
| Delay of the shared encoder from tick to packet | slice 1 |
| Whether CAF gives senders a correct media session for a picture it did not load | slice 3 |
| Whether the receiver page's policy admits a cross-origin WebTransport session | slice 3 |
| Whether a `cast` output is admitted to the effective-output registry and has the no-reorder proof | slice 2 |
| Where the dispatcher attaches the per-component identity change list to video and audio packets, and whether every unrequested change can mint an identity (decision 5.4) | slice 2 design note; the contract and its tests are fixed |
| What the Cast platform does when `androidReceiverCompatible` is set and the app is not installed | slice 6, first device step |
| Whether `supportedAppTypes` in the engine-side LAUNCH selects the Android TV app | slice 6 device test |
| Whether UDP 443 and UDP 9010 pass from the network the Cast devices are on | slices 4 and 6 device tests |
| How libsrt 1.5.7 reaches the container images and CI | slice S, lane `srtfloor`. Its plan as told to this team: build 1.5.7 from the pinned upstream tarball in the images |
| Minimum Android API of Media3 and the Cast TV library | slice 6 |
| Whether the audio tap carries the engine timestamp of each block (needed to stamp Opus with engine time) | slice 5 |
| Whether the engine's SRT output writes `PTS = engine time` exactly (libav adds twice `max_delay`, default 0) | slice 6: demux the engine's own output and compare |
| Whether Media3 reports the extractor's sample time unchanged to the frame listener for a progressive source | slice 6 instrumented test; until it passes, SRT latency is `unavailable` |
| Whether libsrt discards, and neither grows nor blocks, when the app stops reading | slice 6 test, before the rung is enabled |
| Whether `jitterBufferTarget` exists on a Cast device and what the jitter buffer then holds | slice 5 device test |
| Existing `webrtc` output: RTP time becomes 0 for every frame after 2^32 (video after 13 h 15 min, audio after 24 h 51 min; read in code, not reproduced) | issue #492, ship lane `rtpwrap`; outside this ADR |
| What the app does when ladder calls for a fresh SRT selector run into the rate limit during rapid reconnects (review note R2-N2) | slice 2 design note: retry after the limit, and HLS after 3 refused calls |
| RTCP wall clock for WebRTC when the run has no validated epoch (review note R2-N3) | slice 5: fall back to the time of writing for both streams, and report latency as `unavailable` |
| The ladder's answer when no registry subscription is left for MoQ (review note R2-N4) | slice 4 test: the answer names the next rung; it is never an error |

