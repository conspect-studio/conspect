# multiview-output — agent notes

Output sinks/servers: RTSP server, HLS/LL-HLS packager, NDI out, RTMP/SRT push. No FFmpeg LL-HLS muxer to lean on — that is ours.
Inv #7 — encode-once-mux-many: composite once, encode the canvas once per rendition, fan the same packets to all transports. No per-tile re-encode, no ABR-per-tile.
Inv #3: re-stamp all output PTS/DTS from the tick counter — never pass raw input PTS to the muxer.
Inv #1/#10: the muxer/transport layer must never stall the output clock or let a slow client back-pressure the engine; bounded drop-oldest on the way out.
Licensing: default build stays LGPL-clean (`gpl-codecs` opt-in → GPL); `ndi` feature is runtime-loaded, never vendored.
Read first: [streaming-gotchas §4](../../docs/research/streaming-gotchas.md).
