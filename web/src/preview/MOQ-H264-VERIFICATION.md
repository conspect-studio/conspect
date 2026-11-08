# Genuine H264 / Opus interoperability and timing

2026-11-08, Google Chrome for Testing149.0.7827.55. Fixture parser commit edc6daa1 on shared source4c6c0613240b094ad32bfe98ed91d07fa8512141. Built only fixture binary; four focused parser/protocol tests passed.

`python3 web/scripts/generate-h264-fixture.py <local fixture.h264packets>` invokes existing host ffmpeg/libx264 once to create30 all-IDR AnnexB access units, baseline320x18030fps with SPS/PPS in each. This is a test generator, NOT a dependency or encoder shipped/default-enabled in the application. No ffmpeg binaries, GPL source, or encoder libraries are vendored by this change.

Run existing publisher with explicit16 or18 and this file plus existing raw Opus fixture; run `node web/scripts/moq-fixture-probe.mjs <test certificate hash> --audio` against isolated fixture page. Actual QUIC-only headless player rendered H264 VideoFrames on both protocols, not merely isConfigSupported:

- draft16:30 paint calls,29 unique advancing video PTS,54 native Opus outputs.
- draft18:30 paint calls,29 unique advancing video PTS,55 native Opus outputs.

Audio and video PTS share the same epoch. `analyze-moq-timing.py` matches nearest audio PTS within10ms of video PTS and compares performance.now() callback times.28 matched observations each: video paint followed native audio decode by111.6–146.9ms on16 and110.7–147.0ms on18. Audio decode is before its playback buffering: these are measured callback deltas, NOT speaker/display synchronization, acoustic latency, or a guaranteed A/V bound. Initial repeated video paint is reported, not counted as an additional unique frame. Headless non-isolated page used postMessage audio buffering. Audible/render synchronization remains unverified and cannot be inferred from these observations.

HEVC remains natively unsupported in measured Chrome149 (earlier exact capability evidence). No HEVC success, production factory, Cloudflare delivery, paid or encrypted media claim. No full web/suite build repeated. All owned fixture/devserver processes stopped after collection.
