# multiview-input — agent notes

Ingest sources (RTSP/HLS/TS/SRT/RTMP/NDI/file/test), custom input pacer, jitter buffers, timestamp normalization, supervised reconnect. Inputs are sampled, never pacing — must never block or back-pressure the engine.
Inv #3: per-input PTS normalized (unwrap 33-bit, genpts fallback, monotonic guard), rebased onto one ns timeline. Carry time as i64 ns/exact rationals — never float fps.
Inv #4: live/VOD-as-live inputs paced to wall-clock by PTS via the custom pacer. `-re` is for files, NOT live ingest.
Inv #2: write lock-free single-slot stores; compositor reads latest-or-placeholder, never blocks. Tiles ride LIVE→STALE→RECONNECTING→NO_SIGNAL.
Bounded queues drop, never grow. No `unwrap`/`panic!` on the ingest hot path — reconnect instead.
Read first: [streaming-gotchas §1–§3,§5–§7](../../docs/research/streaming-gotchas.md).
