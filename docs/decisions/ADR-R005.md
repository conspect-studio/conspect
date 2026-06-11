# ADR-R005: Discrete per-input audio routing + program bus, with a verified per-output capability matrix and explicit degradation

- **Status:** Proposed
- **Area:** Resilience & A/V
- **Date:** 2026-06-02
- **Source brief:** [resilience-and-av.md](../research/resilience-and-av.md)

## Decision

Pipeline: per-input decode -> aresample to 48kHz/common layout (async=1 + first_pts) -> anullsrc silence-fill on dropout -> fan out to (a) clean discrete per-input track and (b) an amix+loudnorm(EBU R128 single-pass) program bus, all PTS-locked to the program clock. Carry discrete tracks natively where supported: MPEG-TS/SRT/RIST (N PIDs), RTSP (N m=audio subsessions, simultaneous), HLS/DASH (N renditions, select-one). NDI = channel-map (input k -> ch 2k,2k+1) or N senders, NEVER N tracks; AAC-over-NDI capped at 2ch (use PCM/Opus). RTMP = tiered/capability-gated: negotiate Enhanced-RTMP v2 multitrack per endpoint, else degrade explicitly to the mixed bus. A machine-readable capability matrix gates the UI/validator. Discrete tracks are decode->re-encode normalized (not passthrough) to survive mid-stream codec changes.

## Rationale

Verified limits: TS/SRT/RTSP carry N simultaneous tracks; HLS/DASH are select-one; NDI is ONE multiplexed stream (up to 255 Opus / unlimited PCM channels, planar FLTP) with no selectable tracks (refuted 'N tracks'); RTMP is NOT categorically incapable (refuted) -- Enhanced-RTMP v2 + FFmpeg flvenc (merged late 2024) support multitrack, but real delivery is gated by endpoint (Twitch <=2, YouTube ignores, IVS video-only). anullsrc silence-fill keeps tracks gap-free during input loss (load-bearing for invariant A).

## Alternatives considered

Promising N tracks on RTMP/NDI unconditionally (silently breaks requirement B); always-mixed-only (loses discrete requirement); passthrough discrete tracks (breaks on mid-stream codec change).

## Consequences

Requirement B met on the right formats; honest, validated degradation elsewhere. UI must show channel-map for NDI and endpoint-negotiated capability for RTMP. Receiver-dependent TS/SRT first-PID-only behavior must be documented and tested (verify all PIDs reach a cooperating receiver).
