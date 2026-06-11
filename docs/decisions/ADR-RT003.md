# ADR-RT003: Snapshot-then-delta with per-connection seq resume and re-snapshot fallback

- **Status:** Proposed
- **Area:** Realtime API
- **Date:** 2026-06-02
- **Source brief:** [realtime-api.md](../research/realtime-api.md)

## Decision

On subscribe, send one $snapshot per topic (full current state, optionally ids-filtered, with the seq/ts it is current as of) read from engine watch channels, then stream only deltas. Each frame carries a per-connection (per-topic) monotonic seq. Keep a small bounded per-session/per-topic replay ring of serialized envelopes. On reconnect the client presents last_seq (WS $resume / SSE Last-Event-ID): if in the ring, replay the gap; if evicted or the session is unknown, send $resync + a fresh snapshot and a new seq baseline. Emit $lag proactively on this-connection overflow to trigger a targeted re-snapshot. Exclude high-rate meters from the ring (latest-only). Sessions expire after a ~30s TTL.

## Rationale

Snapshot+delta keeps clients consistent with no REST polling and is cheap because the engine already holds latest state in watch channels. Per-connection seq + a tiny ring gives lossless short-resume and cheap re-snapshot for everything else with trivially bounded O(connections × ring) memory — right-sized for a single-process live engine. Self-healing via $lag bounds memory and keeps correctness without a global event log.

## Alternatives considered

Global Kafka-style event log (over-engineered for single-process, couples connections, unbounded growth); resume-everything (unbounded memory); no resume / always re-snapshot (bandwidth/CPU spikes on flaky links, loses brief-disconnect continuity); client-side polling (latency, load, races).

## Consequences

Short resume is lossless but a long disconnect or server restart forces a full re-snapshot — the UI MUST treat $resync as a state REBUILD, not a merge (must be documented + tested). No cross-topic global ordering; atomic multi-resource changes must be coupled into one delta or reconciled via ts. Ring sizing is a tradeoff (too small = frequent re-snapshots; too large = memory).
