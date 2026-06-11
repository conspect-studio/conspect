# ADR-M002: EncodeProfile + transcode model: composite-once, scale-per-output, capability-gated backends, pinned vs hot params

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Model transcoding as a reusable EncodeProfile (codec/backend/profile/level/resolution+max/fps/pixfmt/bit-depth/chroma/rate-control/preset/tune/GOP/bframes/lookahead/refs/slices/tiles/multipass/SFE/intra-refresh/latency-profile) referenced by Outputs and Renditions. The canvas is composited once at canvas resolution; each Output/Rendition does a GPU scale then one encode session. Outputs sharing canvas+rendition+codec+bitrate get free packet fan-out; any differing rendition is a separate encode session counted against the runtime-probed NVENC session budget. Encoder backend defaults to 'auto' (HAL scored negotiation, platform fixed-function first, software fallback). Every codec/profile/level/session option is gated by CapabilityReport. A one-click latency_profile bundle (low_latency/quality_vod/custom) maps to per-backend flags. Pinned params (codec/profile/level/pixfmt/bit-depth/chroma/GOP-structure/max-resolution) are Class-2 (parallel-output migration); reconfigurable params (fps/rc-mode/bitrate/maxrate/bufsize/preset where supported) are hot via NvEncReconfigureEncoder.

## Rationale

Composite-once-scale-per-output is the efficient architecture and makes the free-fan-out vs separate-session capacity cost explicit and manageable. Capability gating prevents offering hardware-impossible encodes. The pinned/hot split is the mechanism that delivers bulletproof live reconfiguration.

## Alternatives considered

Per-output independent composite (rejected: N x GPU cost); free ABR ladder assumption (rejected: fan-out is free only when codec+res+bitrate match); hardcoding the NVENC session cap (rejected: moving, host-wide, per-driver number — must probe nvmlDeviceGetEncoderSessions); single global encoder (rejected: cannot serve heterogeneous protocols/renditions).

## Consequences

UI must show a live session-budget calculator (renditions + hot-standby + preview) and badge shared-vs-separate encode. VideoToolbox cannot change resolution live (always Class-2 on macOS) and its HEVC low-latency is unconfirmed, so the mac low-latency path is steered to H.264. Each distinct rendition/preview/hot-standby consumes a session and must be admission-counted.
