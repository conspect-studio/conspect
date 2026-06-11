# ADR-E004: Encode-once, mux-many fan-out (tee semantics); ladder is a separate cost

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Fan the same compressed bitstream to all transports (RTSP/HLS/SRT/RTMP) in-process (one encoder -> N protocol muxers receiving packet copies), each output isolated behind its own thread + bounded queue + failure-ignore so a slow/flaky sink cannot back-pressure the encoder. Treat same-codec+resolution+bitrate fan-out as free; treat any differing rendition (resolution or codec) as a separate scale+encode. If an ABR ladder is required: composite once, split the canvas, scale on-GPU per rung, one encoder session per rung, GOP-aligned closed GOPs across rungs.

## Rationale

Verification CONFIRMED tee operates on already-encoded packets (free fan-out) and that this does NOT extend to an ABR ladder — different resolution/codec forces decode/composite-shared but per-rung scale+encode. The decode/composite is the shared win; scale+encode is the per-rendition cost.

## Alternatives considered

Per-output FFmpeg instances — rejected: multiplies encode + adds IPC. Assume the ladder is near-free after the top rung — rejected (verified false): each rung is a capacity-bounded encode session.

## Consequences

Must implement per-output fifo/back-pressure isolation and onfail recovery ourselves. Rung count is a first-class resource budget counted against encoder-session caps (NVENC per-system; QSV/VAAPI engine throughput; Apple media engines). Keep per-rung scale on-GPU to avoid VRAM<->RAM copies.
