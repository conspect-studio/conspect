# ADR-P002: Default per-scope transport: cheap JPEG grids, on-demand WHEP focus, LL-HLS fallback

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-02
- **Source brief:** [preview-subsystem.md](../research/preview-subsystem.md)

## Decision

Per scope, default to the cheapest transport for grids and reserve WebRTC/WHEP for a single on-demand focus. INPUT grid: MJPEG-over-HTTP / single-shot JPEG at 1-5 fps, ~320x180, encode-once-serve-many. PROGRAM grid/at-a-glance (default): multiplexed binary JPEG over ONE WebSocket (sidestepping the browser ~6-conns/host cap), 1-5 fps, CPU JPEG (turbojpeg/zune-jpeg), no GPU encode session. OUTPUT grid: periodic JPEG snapshots (1-5 s, ETag) of the REAL decoded rendition, viewport-driven. FOCUS (all scopes): one WebRTC/WHEP session at a time per operator, sub-second, using a session-budgeted preview encoder; opening a second focus demotes the first to JPEG. FALLBACK where WebRTC cannot establish (UDP/STUN/TURN blocked): LL-HLS reusing the multiview-serve CMAF segmenter; for HLS-family outputs, replay the output's own published playlist at zero extra cost. WHEP is feature-gated and advertised via the capabilities endpoints. On base Apple silicon (1 encode engine) prefer JPEG and restrict/queue WHEP.

## Rationale

Latency need is mode-specific: grids need at-a-glance liveness (cheap, high fan-out), only a focused view needs sub-second. JPEG over a single multiplexed WS scales to a many-tile web grid without exhausting browser connection limits or encoder sessions; WHEP delivers the only sub-second path but is expensive (a real encoder session), so it must be bounded to one focus. Reusing the existing CMAF segmenter for LL-HLS and replaying published HLS segments for HLS outputs adds true-consumer fidelity at near-zero marginal cost. Feature-gating + capability advertisement lets the UI degrade cleanly where WebRTC/TURN is unavailable.

## Alternatives considered

Per-tile WebRTC for the whole grid (rejected: exhausts browser conns + encoder sessions). Raw per-stream MJPEG for the program multiviewer (rejected: hits the ~6-conns/host cap). LL-HLS everywhere (rejected: ~2-5 s latency unacceptable for focus motion/lipsync judgement). WebRTC as the only path (rejected: fails on restrictive networks and base-Apple session limits).

## Consequences

Three transport stacks to build/maintain (JPEG, WHEP, LL-HLS) plus the WS multiplexer, but LL-HLS reuses multiview-serve and WHEP can reuse the MediaMTX sidecar. The UI must implement transparent transport fallback (WHEP -> LL-HLS -> JPEG) and surface a small fallback note. Focus is capped at one per operator, which is the intended ergonomic anyway.
