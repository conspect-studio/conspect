# ADR-E005: Reference-counted frame pool with bounded, drop-oldest working set

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Reference-count and pool every frame/surface (AVFrame/AVBuffer + av_buffer_pool; bytes::Bytes for CPU fan-out; object-pool/slab/bumpalo for host buffers; gpu-allocator/cudaMallocAsync pools for GPU scratch) wrapped in pooled handles that recycle on last drop. Bound every inter-stage queue to depth 1-3 with drop-oldest/latest-frame-wins (per-tile capacity-1 slot). Share one GPU/CUDA context across all decoders. Size decode pools minimally per actual content (NVDEC ulNumDecodeSurfaces=min+3..4, ulNumOutputSurfaces=1-2, ulMaxWidth/Height = real content; VAAPI/QSV init_pool_size covering codec ref frames). Set the global allocator to mimalloc.

## Rationale

Per-frame malloc/free of multi-MB buffers fragments and stalls over hours; unbounded queues are the canonical OOM failure mode. Verification stressed that plain Bytes/AVFrame refs free to the global allocator, not the pool, unless wrapped; that one shared CUDA context amortizes ~84-115 MB once; and that inflated ulMaxWidth/Height balloons a 1080p decoder from ~41-53 MB to ~542 MB.

## Alternatives considered

Plain Vec<u8>/system allocator per frame — fragments and churns. Deep bounded queues with blocking backpressure — rejected for live: blocking a decoder breaks continuous output; drop instead. Per-decoder CUDA context — multiplies fixed cost by N.

## Consequences

Pooling needs explicit recycle logic, not just refcounting. Refs must be dropped early — a slow sink holding refs drains the pool. Working set modeled as N x (1 pending + 1 in-composite) small NV12 frames + output, plus B-frame reorder/sink-jitter pins.
