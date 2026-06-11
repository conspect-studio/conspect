# ADR-MV003: Add loudness logging and multi-standard audio metering for compliance

- **Status:** Proposed
- **Area:** Broadcast Multiviewer
- **Date:** 2026-06-02
- **Source:** [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md)

## Decision

Extend EBU R128 metering into a compliance-grade audio subsystem: expose all loudness sub-meters (momentary 400 ms, short-term 3 s, integrated/gated, Loudness Range, max true-peak) with a selectable target profile (EBU R128 -23 LUFS or ATSC A/85 -24 LKFS), selectable meter ballistics/scales (PPM Type I/IIa/IIb per IEC 60268-10, VU, digital sample-peak per IEC TR 60268-18, true-peak dBTP per ITU-R BS.1770), phase/correlation + goniometer and surround grouping with Lo/Ro-Lt/Rt downmix metering, and per-channel + program-bus loudness. Persist loudness as a queryable time-series log with charts and exportable (CSV/PDF) compliance reports, and feed out-of-spec excursions and dialnorm mismatches into the ADR-MV001 alarm engine. Loudness CORRECTION/normalization and audience-measurement watermark reading are explicitly NOT implemented.

## Rationale

Loudness logging and multi-standard metering are near-universal in 24/7 monitoring multiviewers and are the legal proof-of-compliance artifact for CALM Act / R128 / A/85. Multiview already measures R128 live; the delta is multi-profile selection, richer meter types and persistence/export — all DSP/storage on the decoded PCM Multiview already has.

## Alternatives considered

Live meters only without logging (rejected: no compliance evidence trail); single R128 profile (rejected: ATSC A/85 and regional targets are required for other markets); real-time loudness correction (rejected: a separate processing product, not multiviewer monitoring).

## Consequences

Requires a retained time-series store with defined retention and export, plus report templates. Meter standard numbers must be cited precisely (true-peak belongs to ITU-R BS.1770, not IEC TR 60268-18, which is a Technical Report for digital sample-peak). AC-3/E-AC-3 (ATSC A/52) metadata decode for dialnorm comparison may require licensing, so default to detect/passthrough.
