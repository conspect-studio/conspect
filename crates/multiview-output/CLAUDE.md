# multiview-output — agent notes

Output sinks/servers: RTSP server, HLS/LL-HLS packager, NDI out, RTMP/SRT push. There is **no
FFmpeg LL-HLS muxer to lean on — that is ours.**

- **Inv #7 — encode-once-mux-many:** composite once, **encode the canvas once per rendition**,
  fan the *same* packets to all transports. Separate encode **only** when codec/res/bitrate
  differ. No per-tile re-encode, no ABR-per-tile (explicit non-goals).
- **Inv #3:** re-stamp all output PTS/DTS from the **tick counter** — never pass raw input PTS
  to the muxer.
- **Inv #1/#10:** the muxer/transport layer must never stall the output clock or let a slow
  client back-pressure the engine; bounded drop-oldest on the way out.
- **Licensing:** keep the default build LGPL-clean (`gpl-codecs` opt-in → GPL); the `ndi`
  feature is **runtime-loaded** (`NDIlib_v6_load()`), never vendored, attribution mandatory.

Read first: [streaming-gotchas §4](../../docs/research/streaming-gotchas.md) ·
[core-engine §9.2](../../docs/research/core-engine.md) · ADR-0006/0007/T005 ·
ADR-E003/E004 (fan-out). Map: [codebase-map](../../docs/development/codebase-map.md).
