# ADR-MV001: Add a content-aware monitoring/alarm engine with X.733 severity and northbound notification

- **Status:** Proposed
- **Area:** Broadcast Multiviewer
- **Date:** 2026-06-02
- **Source:** [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md)

## Decision

Add a probe-driven monitoring engine inspecting DECODED essence (not just transport state) per source/tile: black and freeze (configurable luma/diff threshold, detection-zone, dwell), audio silence/over/clip/phase/imbalance, loudness violation + dialnorm mismatch, caption presence-loss, format/standard change, AFD mismatch, colorimetry/HDR mismatch, optional no-reference QoE (open metrics only — ITU-T P.1203/P.1204), ETSI TR 101 290 Priority 1/2/3 for MPEG-TS, Media Delivery Index (RFC 4445) + ST 2022-7 per-path health for RTP, and SCTE-35/104 cue monitoring. Every probe has threshold + dwell-up + dwell-down + auto-clear (hysteresis). Alarms carry ITU-T X.733 perceived severity (Critical/Major/Minor/Warning/Cleared) with probe->tile->group->system roll-up, latch, operator acknowledge and virtual/Boolean grouping (AND/OR/XOR). Severity drives the existing tally_border/box/alert_card colour and a penalty-box auto-promote-on-fault layout action. Northbound delivery is SNMP traps (+ MIB), syslog (RFC 5424), email and webhook, layered on the existing REST/WebSocket API, with persistent alarm + loudness logging and availability/error-second reporting.

## Rationale

This is the single biggest capability gap between a compositor with a transport-level LIVE/STALE/NO_SIGNAL state machine and a professional broadcast multiviewer, and it is near-universal and standards-anchored (X.733, TR 101 290, RFC 4445/5424, SNMP). It reuses Multiview's existing per-tile state machine, alert_card overlay and R128 metering rather than replacing them.

## Alternatives considered

Keep only transport-level state (rejected: misses black/freeze/silence/loudness/TS faults that define a multiviewer); use closed/non-standard QoE scoring (rejected in favour of open ITU-T P.120x metrics); push raw probe events to the UI without an alarm lifecycle (rejected: flapping, no ack/roll-up, no NOC integration).

## Consequences

Adds continuous CPU/GPU cost for decoded-frame analysis — probes run at reduced cadence/resolution and are individually disableable to protect the output-clock invariant. Requires a persistent event/loudness store (retention + export) and an SNMP MIB to maintain. TR 101 290 applies only to TS inputs; MDI severity bands are implementation-defined and documented as such.
