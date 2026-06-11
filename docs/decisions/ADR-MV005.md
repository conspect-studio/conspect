# ADR-MV005: Adopt NMOS (IS-04/05/07/08, IS-10, IS-12) and router-control bridges (Ember+, SW-P-08) for IP-facility integration

- **Status:** Proposed
- **Area:** Broadcast Multiviewer
- **Date:** 2026-06-02
- **Source:** [broadcast-multiviewer-features.md](../research/broadcast-multiviewer-features.md)

## Decision

For the professional IP-facility tier, make Multiview a first-class NMOS device: AMWA IS-04 registration/discovery (each tile a routable receiver, the engine's senders advertised), IS-05 connection management (external controllers patch sources onto tiles), IS-08 audio channel mapping, IS-07 event/tally as the IP-native GPI/tally transport, IS-12/MS-05 device control + the receiver-monitoring status model, and IS-10 OAuth2/JWT to secure control APIs. Provide router/switcher interop bridges via Ember+ (provider/consumer parameter tree exposing layouts/sources/UMD/tally/alarms) and SW-P-08 (route-follow + name-following labels + optional crosspoint/salvo command). These are additive bridges; the existing REST/WebSocket API remains the primary control surface, and NMOS/router support is conditional on the ST 2110 ingest work (M12).

## Rationale

NMOS IS-04/05 is the de-facto control plane of ST 2110 facilities and is becoming table-stakes for IP multiviewers; Ember+ and SW-P-08 are the established interop protocols for mixed/legacy routed facilities. Supporting them lets a broadcast controller assign sources to Multiview tiles and keep labels/tally correct on route changes using only openly-published interop protocols.

## Alternatives considered

Proprietary REST API only (rejected: cannot be discovered/controlled in an NMOS facility); native-only greenfield NMOS without SW-P-08/Ember+ (rejected: ignores the large installed base of routed plants); cloning a vendor control system (rejected: interoperate via open protocols instead).

## Consequences

NMOS implies the ST 2110 + PTP timing stack and adds significant surface (registry interaction, IS-05 SDP handling, IS-10 auth-server dependency). The mapping of the IS-12/MS-05 receiver-monitor model onto MV tiles is an inference not mandated by AMWA and is documented as such. SW-P-08 and Ember+ behaviours vary by vendor, so drivers need per-platform profiles.
