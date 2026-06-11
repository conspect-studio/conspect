# ADR-E001: Decode each tile at (or near) its display resolution, per-backend negotiated

- **Status:** Proposed
- **Area:** Efficiency
- **Date:** 2026-06-02
- **Source brief:** [efficiency.md](../research/efficiency.md)

## Decision

Negotiate decode-time/early downscale per (backend, codec) into one of four tiers and never carry a tile at more pixels than displayed. NVIDIA: cuvid -resize on the *_cuvid decoders (fused, ASIC-free, YUV output, one resolution per session; second tile size via on-GPU scale_cuda/scale_npp). Apple native: direct VTDecompressionSession + kVTDecompressionPropertyKey_ReducedResolutionDecode, probed per codec, best-effort. Intel/AMD: post-decode SFC/VPP scale_qsv/scale_vaapi on the media block (decoder still emits full-res reference frames). Software: no decode-time downscale for H.264/HEVC/VP9/AV1 (lowres no-ops); use frame skipping. Deduplicate sources and resize once to the largest consuming tile. Prefer requesting lower-res source renditions/substreams where the protocol offers them. Budget decode-engine load by SOURCE megapixels/sec.

## Rationale

Decode-at-display-resolution is the single highest-leverage memory/bandwidth lever (savings scale with the square of the downscale) and is decisive on bandwidth-bound iGPUs/APUs/Apple silicon. Verification REFUTED the blanket 'decode-time downscale does not exist' claim — it is real on NVDEC and VideoToolbox-native — and CONFIRMED it is post-decode-only on the FFmpeg VAAPI/QSV path and absent in software for modern codecs.

## Alternatives considered

(a) Assume universal decode-time downscale — refuted; would silently waste decode bandwidth on VAAPI/QSV/FFmpeg-VideoToolbox and OOM on full-res-fallback tiles. (b) Always decode full-res then GPU-scale — wastes the most-constrained resource (memory bandwidth) on every iGPU. (c) Assume it never exists — leaves the biggest win unused on exactly the NVIDIA-entry and base-Apple tiers that matter most.

## Consequences

HAL must carry a per-backend decode-scale capability matrix and budget the WORST realized path per tile. On FFmpeg VAAPI/QSV/VideoToolbox and software fallback, reserve a full-res surface set per stream in the VRAM/RAM budget. True decode-time reduction on Apple requires a custom VTDecompressionSession (not FFmpeg) and per-codec probing. Tile geometry must be even/4px-aligned for cuvid crop/resize.
