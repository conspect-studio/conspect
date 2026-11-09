# Retained-frame serialization regression

2026-11-08: rebuilt only `cargo build --manifest-path tools/moq-fixture/Cargo.toml --bin conspect-moq-fixture` from shared HEAD fe7fe924eec120f1a4952b703c3f29521fb9c1ae (includes a148fb4d serialized-object copy repair). No uncommitted media/fixture source changes at build. Browser remains Chrome149.0.7827.55, headless @moq/watch0.6.6 with @moq/net0.4.7. Media owner confirmed subsequent timescale edits are tests only, production wire unchanged.

Ran the same binary with explicit draft16 then draft18 at https://localhost:4443, VP8 IVF plus length-prefixed Opus packets. Invoked `node web/scripts/moq-fixture-probe.mjs <test certificate SHA256> --audio` against isolated Vite [::1]:5179. No full web build or external relay resource creation.

- draft16:31 painted video frames (31 unique advancing PTS),49 native audio decoder outputs, audio PTS86720000..87680000us.
- draft18:30 paint calls (29 unique advancing video PTS; initial frame painted twice),54 native audio decoder outputs, audio PTS89020000..90080000us.
- Native Opus spacing20ms (occasional ±1us decoder rounding). Both negotiated WebTransport-only requested draft and decoded the hang timestamp+payload mapping after contiguous serialization change.

Logs are adjacent `moq-retained-draft16.log` and `moq-retained-draft18.log`. These observations prove local wire decode interoperability, NOT rendered-speaker A/V synchronization, encrypted or paid playback, or Cloudflare delivery. Non-isolated test page warned of higher-latency postMessage audio buffer. All owned fixture/devserver processes stopped after collection.
