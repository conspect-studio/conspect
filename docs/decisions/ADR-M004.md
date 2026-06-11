# ADR-M004: Audio track-mapping model: Source owns attributes, Output owns the cross-product mapping, capability-aware projection

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Split audio ownership: the Source owns per-input audio attributes (track selection, gain, mute, include-in-program-bus, resample/silence-fill, metering). The Output owns the program bus (codec/channels/SR/bitrate/label + EBU R128 loudnorm) and the discrete per-input track set (each track: input+source-channels, codec/channels/SR/bitrate, label/language/default, include-in-program/program-gain/program-mute). Discrete tracks are always clean (decode->re-encode normalized, never gain-altered, anullsrc silence-filled). The carrier projection (TS PID / RTSP m=audio subsession / HLS-DASH select-one rendition / NDI channel-map-or-multi-sender / E-RTMP trackId) is derived read-only from a first-class capability matrix; the routing matrix UI (inputs x tracks/channels) greys impossible cells and shows any degradation taken. Track LAYOUT (count/identity, per-track codec/channels/SR) is pinned (Class-2); bitrate/gain/mute/labels are hot.

## Rationale

Separating per-input attributes from the cross-product mapping eliminates the duplicated/conflicting-control risk flagged across the Input and Output areas. Capability-aware projection prevents silently dropping tracks on carriers that cannot carry them (RTMP=1, NDI=channels, HLS=select-one).

## Alternatives considered

Put all audio mapping in the Source (rejected: an input feeds many outputs with different track sets); put track selection in the Output only (rejected: ingest track/gain/metering are per-input properties); assume N-in->N-out works on every protocol (rejected: carrier asymmetry is real and must be surfaced).

## Consequences

RTMP multitrack and SRT/RIST multi-PID are receiver/endpoint dependent (Twitch <=2, YouTube ignores, many SRT decoders take first PID), so capability must be negotiated per endpoint at connect and the actual degradation shown. NDI requires a forced channel-map/multi-sender branch with no discrete-track illusion.
