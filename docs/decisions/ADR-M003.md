# ADR-M003: Color-control model: four-axis per-source override + single canvas working space + output CICP tagging with verify gate

- **Status:** Proposed
- **Area:** Management
- **Date:** 2026-06-02
- **Source brief:** [management-capability-matrix.md](../research/management-capability-matrix.md)

## Decision

Manage color as four independent CICP axes (primaries, transfer/TRC, matrix, range) plus chroma siting and bit depth across three stages. (1) Ingest: detection precedence frame>codec>container>policy yields a never-UNSPECIFIED resolved tuple exposed read-only with per-axis provenance (GET /api/v1/sources/{id}/color); per-source overrides (PATCH color_override.*) apply as per-frame compositor kernel uniforms (hot, no reset) plus per-source HDR->SDR tone-map. (2) Canvas: a single working_color_space (sdr-bt709-limited default; hdr-pq-bt2020/hdr-hlg-bt2020; custom axes) drives gamut-convert/tone-map stages and required bit depth — changing it while bound is the one inherently non-hot media path (reset borne by outputs). (3) Output: CICP tags written to VUI/SEI+container default to the canvas working space, must set all four axes (never CICP=2), are relabel-only/hot, with HDR explicit-only (couples PQ/HLG+BT.2020+10-bit+main10), in-band SEI forced for ES transports, and a mandatory post-encode+post-remux ffprobe verify gate that fails on unknown/mismatch.

## Rationale

Untagged/mistagged feeds are the central color trap; per-axis override with visible provenance lets operators fix feeds by eye. A single canvas working space avoids the one-HDR-tile-washes-out bug and ambiguous multi-space compositing. Tagging-never-converts plus a verify gate is the only way to guarantee output color is not silently wrong (zero errors otherwise).

## Alternatives considered

Single combined color enum (rejected: cannot express the real mixed mis-tag combinations); per-output working color space (rejected: re-composites N times and reintroduces multi-space ambiguity); trusting encoder defaults (rejected: encoders write CICP=2 -> players re-guess; NVENC/VT range pitfalls); auto-promoting HDR from resolution (rejected: catastrophic PQ-on-SDR).

## Consequences

HW color-metadata propagation is driver/version dependent (VAAPI unconfirmed, VT range bug) so the verify gate is load-bearing and re-runs after every remux (TS<->MP4 drops colr). HDR is gated behind expert confirmation with interop-risk warning. The UI must surface detected-vs-override and warn on PQ/HLG-on-SDR and full-vs-guessed-limited.
