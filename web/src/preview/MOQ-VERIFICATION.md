# MoQ browser verification (2026-11-08)

Chromium executable `~/.cache/ms-playwright/chromium_headless_shell-1228/chrome-headless-shell-linux64/chrome-headless-shell --version` reports Google Chrome for Testing 149.0.7827.55.

- Final VP8 shared Window fixture decoded and painted 30 advancing VideoFrames each on draft16 and draft18 (adjacent fixture logs).
- Draft18 A/V fixture decoded 49 native Opus AudioData outputs (48kHz stereo) and painted 30 VP8 frames, with increasing timestamps on the common media epoch. Audio decode runs ahead of video render; this is NOT a measurement of speaker/display AV synchronization. The non-cross-origin-isolated test page used the higher-latency postMessage audio buffer.
- Native capability probe reports VP8/H264/Opus/AAC supported; HEVC hvc1/hev1 unsupported. Support queries do not prove decode. No H264/HEVC actual playback claim.
- Run `node web/scripts/moq-fixture-probe.mjs <test-certificate-sha256> --audio` against the optional Opus fixture on localhost4443 and Vite localhost5179. Certificate pin injection exists only on the test page. Real controls never accept an untrusted pin.
- Fixture Opus input is bounded u32-LE length-prefixed raw packets, 20ms each, generated once from ffmpeg sine48k stereo/libopus and extracted from Ogg; no per-viewer encoding.

## Native-only distribution gate

`@moq/watch` is MIT OR Apache-2.0. Its `@moq/hang` dependency normally dynamically imports LGPL libav wrappers. Merely leaving audio muted does not remove distribution duties. Vite now replaces ONLY hang's libav loader with the native-codec capability shim and fails the build if any libav decoder module remains in an output chunk. Measured production bundle contains no libav assets. Node dependency trees still contain those packages: do not distribute node_modules or upstream artifacts without their licenses, notices and corresponding-source obligations. This engineering gate does not replace legal review or alter legal content.

## Limits

The above tests are local fixture interoperability evidence, not Cloudflare authenticated delivery, paid media, encrypted playback, real output-engine integration, or audible AV-sync acceptance. Those gates belong to their separate work owners.
