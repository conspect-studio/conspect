# ADR-M007: CapabilityReport as the single machine-readable gate for UI and validator

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Expose a read-only CapabilityReport (GET /api/v1/system/capabilities and sub-resources) assembled from L1 (FFmpeg portable), L2 (vendor deep queries: NvEncGetEncodeCaps/cuvid/oneVPL/VAAPI/VTCopyVideoEncoderList) and L3 (sandboxed probe), correlating devices by PCI bus id / UUID / IOKit id (never NVML index). It reports detected GPUs, per-(stage x backend) availability (compiled-in vs probed), per-(device x codec) decode/encode caps (max-res/level/profiles/chroma/bit-depth/B-frame support), the runtime-probed NVENC/VideoToolbox session caps (live used/available, host-wide), VRAM, host cpu/cgroup/ram/PSI, and effective license/build. The same object gates every codec/profile/level/session control in the UI (impossible options greyed with reason) and is the validator's rejection source — never two sources of truth.

## Rationale

Output and policy controls must never offer something the hardware cannot do; a single probed report shared by UI and server validator guarantees consistency and prevents over-admission/session-create failures mid-operation.

## Alternatives considered

Hardcode capabilities per known GPU (rejected: caps and session caps are moving per-driver numbers); separate UI capability list from server validator (rejected: drift -> UI offers what the server rejects); rely on NVML for codec caps (rejected: NVML cannot report codec capability).

## Consequences

macOS telemetry is coarse (ProcessInfo.thermalState only, no per-VT-session stats), so the adaptive UI on Mac presents lower-confidence thermal-driven degradation and relies on measured encode-ms. The NVENC cap is host-wide across all processes, so the budget calculator must warn it is shared, not exclusive to Multiview. Recalibration/L3 probes briefly load the box and need a 'may impact live output on a loaded host' confirm.
