# multiview-input — agent notes

Ingest sources (RTSP/HLS/TS/SRT/RTMP/NDI/file/test), the **custom input pacer**, jitter buffers,
timestamp normalization, and supervised reconnect. **Inputs are sampled, never pacing** — they
feed last-good-frame stores; they must never block or back-pressure the engine.

- **Inv #3 — unified timing:** per-input PTS normalized (**unwrap 33-bit**, genpts fallback,
  monotonic guard) and rebased onto one ns timeline. Carry time as i64 ns / exact rationals;
  **never float fps** (drifts ~3.6 s/hour). NTSC `1001` as exact rationals.
- **Inv #4 — HLS ingest pacing:** live / VOD-as-live inputs paced to **wall-clock by PTS** via
  the custom pacer. **`-re` is for files, NOT live ingest.**
- **Inv #2:** write lock-free single-slot stores; the compositor reads latest-or-placeholder and
  never blocks. Tiles ride LIVE→STALE→RECONNECTING→NO_SIGNAL.

Bounded queues drop, never grow. No `unwrap`/`panic!` on the ingest hot path — reconnect instead.

Read first: [streaming-gotchas §1–§3,§5–§7](../../docs/research/streaming-gotchas.md) ·
[core-engine §9.1](../../docs/research/core-engine.md) ·
ADR-T003/T004/T006/T007/T008. Map: [codebase-map](../../docs/development/codebase-map.md).
