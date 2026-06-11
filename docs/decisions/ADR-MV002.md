# ADR-MV002: Implement TSL UMD (v3.1/4.0/5.0) ingest/egress with external tally-bus integration and arbitration

- **Status:** Proposed
- **Area:** Broadcast Multiviewer
- **Date:** 2026-06-02
- **Source:** [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md)

## Decision

Implement the royalty-free TSL UMD protocol family as the dynamic-label/tally transport: a listener (and optional sender) for v3.1 (RS-422/485 + UDP/IP, on/off tally), v4.0 (per-element LH/Text/RH colour tally 0=off/1=red/2=green/3=amber) and v5.0 (native 16-bit UDP, ASCII/UTF-16LE, screen+display addressing 0-0xFFFE with 0xFFFF broadcast, multiple displays per packet), with v5.0 over UDP as the primary path. The listen port is configurable (the spec defines none; default v5.0=40003, legacy=40001 as documented conventions); DLE/STX framing applies only on TCP/byte-stream and is disabled on UDP; displays-per-packet are computed against the 2048-byte ceiling, not a fixed 16. TSL drives the existing label overlay (live text, no layout reload) and a generalized multi-region tally (LH/RH/text, amber third state, brightness). A tally arbiter aggregates external sources (switcher PGM/PVW/ME/Aux, router crosspoints, NMOS IS-07, GPI) and resolves per-source PGM/PVW/ISO onto tiles, with a configurable bit-to-colour and display-index-to-tile mapping profile. Name-following binds labels to the routed source identity.

## Rationale

UMD + tally is the most universal, standards-anchored multiviewer capability, and TSL is the open de-facto standard. Multiview already has the rendering primitives (label, tally_border); the gap is purely protocol ingestion/emission and bus-to-tile arbitration. The wire format and version differences were field-verified against the primary TSL spec.

## Alternatives considered

Internal-only tally from Multiview's own state (rejected: a multiviewer must reflect the external on-air bus); a bespoke label/tally API only (rejected: no interop with switchers/routers/controllers); assume a fixed UDP port / 16-char field / always-on DLE-STX (rejected: contradicts the spec — port undefined, v5 text variable-length, framing TCP-only).

## Consequences

Must interoperate with mixed-vendor controllers that deviate on bit semantics, so the bit-to-colour/index-to-tile mapping is configurable and tolerant. v4.0 and v5.0 are not wire-compatible (different CONTROL bit ordering), so both code paths are maintained. Some receivers cap packet size below 2048 bytes, so packet size is conservative/configurable. The arbiter introduces conflict-resolution policy that must be explicit and auditable.
