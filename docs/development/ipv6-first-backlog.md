# IPv6-first — remediation backlog

Dependency-ordered, PR-sized slices to bring the tree to the **IPv6-first** posture
([conventions §10](../architecture/conventions.md) / [ADR-0042](../decisions/ADR-0042.md) /
[ipv6-first](../research/ipv6-first.md)). Each is TDD-first with an **IPv6 (or dual-stack) test**.
Sizes: `S` ≤½ day, `M` ~1–2 days, `L` ~3–5 days. Sites are from the 2026-06-08 audit.

> **Posture:** IPv6-first with **dual-stack `[::]` defaults** (legacy IPv4 still interops during the
> deprecation window) — never IPv4-only, never IPv4-first. None of these change a data-plane
> invariant; IPv6 is an address family, not a pacing change.

| # | Slice | Size | Crate(s) | Depends on | Deliverable + acceptance |
|---|---|---|---|---|---|
| **IPV6-0** | Canonical principle: conventions §10 + ADR-0042 + brief | `S` | docs | — | **(Shipped in this push.)** [conventions §10](../architecture/conventions.md) (source of truth), [ADR-0042](../decisions/ADR-0042.md) (Accepted), [ipv6-first](../research/ipv6-first.md). Acceptance: principle documented + indexed; review gate established. |
| **IPV6-1** | Make the merged multicast/SAP docs IPv6-first | `S` | docs | IPV6-0 | **(Shipped in this push.)** Rewrite the IPv4-led examples → IPv6-first in [multicast-transport](../research/multicast-transport.md), [sap-discovery](../research/sap-discovery.md), ADR-0040/0041 and their backlogs: examples lead IPv6, `c=IN IP6` primary (no TTL), IPv6 multicast SSM is primary (not "a follow-up"), bracketed IPv6 URLs. Acceptance: no IPv4-first framing remains in those docs. |
| **IPV6-2** | SDP parse/generate handles `IN IP6` (the silent-mangle bug) | `M` | `multiview-control` (`nmos/is05.rs:340`), the ADR-0041 `sdp/` model | IPV6-0 | **TDD-first:** a failing test that `parse_sdp_transport` (and the unified `sdp/` model) extracts the address+family from `c=IN IP6 ff3e::1` (today it yields the literal `"IP6"`); then parse both families, applying the **no-TTL** rule for IPv6 (`c=IN IP6 addr[/count]`) vs `/ttl` for IPv4 (RFC 8866 §5.7). Acceptance: IS-05 binds an IPv6 multicast flow; the `is05.rs:603/613` IPv4 cases still pass. |
| **IPV6-3** | Control-plane listen = dual-stack `[::]` by default | `M` | `multiview-config`, `multiview-control`, `multiview-cli` | IPV6-0 | **TDD-first:** default `ControlConfig.listen` becomes `[::]:<port>` bound with `IPV6_V6ONLY=false` (explicitly set, not OS-default); a test asserts an IPv4-mapped client still connects (dual-stack). Update the field docs (`multiview-config/src/lib.rs`) + validation to accept/encourage IPv6. Acceptance: control plane reachable over IPv6 and IPv4-mapped; loopback default `[::1]`. |
| **IPV6-4** | Telemetry syslog/SNMP senders bind `[::]` | `S` | `multiview-telemetry` (`syslog.rs:392`, `snmp.rs:702`) | IPV6-0 | **TDD-first:** the ephemeral sender sockets bind `[::]:0` (dual-stack) and can send to an IPv6 collector; a test sends a trap/log line over IPv6 loopback. Acceptance: syslog + SNMP work over IPv6. |
| **IPV6-5** | WHEP: native bind + SDP answer `IN IP6` | `M` | `multiview-preview` (`whep/native.rs:237`, `whep.rs:275/289/306`) | IPV6-2 | **TDD-first:** the native transport binds `[::1]:0`/`[::]:0`; the SDP answer emits `o=…/c=IN IP6 ::` when the negotiated/candidate family is IPv6 (RFC 8866 — must not advertise `IN IP4` for an IPv6 candidate). Acceptance: an IPv6-candidate offer yields an `IN IP6` answer; existing IPv4 fixtures still pass. |
| **IPV6-6** | HA cluster transport dual-stack bind | `S` | `multiview-engine` (`ha/transport.rs:290`) | IPV6-0 | **TDD-first:** the HA transport binds dual-stack `[::]` and peers over IPv6; config accepts bracketed IPv6 peer addresses. Acceptance: a two-node loopback HA test communicates over IPv6. (off-by-default `ha` feature.) |
| **IPV6-7** | IPv6 multicast join path (`join_multicast_v6` / `MCAST_JOIN_SOURCE_GROUP`) | `M` | `multiview-input` (`st2110/transport.rs:104`) + the ADR-0040 multicast ingest | IPV6-2 | **TDD-first:** a `join_multicast_v6(group: Ipv6Addr, ifindex: u32)` (and/or the family-agnostic `MCAST_JOIN_SOURCE_GROUP` for SSM) beside the existing v4 join; the multicast ingest derives bracketed IPv6 `udp://@[…]`/`rtp://[…]` URLs and joins `ff00::/8`/`FF3x::/32` via MLDv2. Acceptance: an IPv6 multicast loopback (where supported) is joined + received; SSM `FF3x::` uses MLDv2 INCLUDE. |
| **IPV6-8** | IPv6-led example configs + `example-streams.md` | `S` | `docs/reference`, `examples/`, `multiview-config` docs | IPV6-3, IPV6-7 | Rewrite `docs/reference/example-streams.md` + sample configs so every example address is **IPv6 first** (`[::1]`, `ff3e::…`, bracketed literals); any IPv4 form is labelled *legacy*. Acceptance: examples lead IPv6 and parse/validate. |
| **IPV6-9** | IPv6 test coverage + an "IPv4-only" review/lint guard | `M` | workspace tests, CI / xtask | IPV6-3..IPV6-7 | Add IPv6/dual-stack cases across the socket + SDP tests (currently all IPv4); add a CI/xtask grep-gate flagging new `0.0.0.0`/`127.0.0.1`/`Ipv4Addr`-only binds or `IN IP4`-only SDP in non-test code (with an inline-justified allow for genuine legacy-only paths). Acceptance: the guard fails a deliberately-IPv4-only fixture; real code passes. |

## Sequencing notes

- **Shipped in this push (docs):** IPV6-0 (principle) + IPV6-1 (multicast/SAP docs made IPv6-first).
  The rest are code remediation, sequenced below.
- **Critical path:** IPV6-2 (SDP `IN IP6`) unblocks IPV6-5 (WHEP answer) and IPV6-7 (IPv6 multicast
  join, which the ADR-0040/0041 media path consumes). IPV6-3 (dual-stack control bind) and IPV6-4
  (telemetry binds) are independent quick wins after IPV6-0.
- **Dual-stack, not IPv6-only:** every bind slice defaults to `[::]` with `IPV6_V6ONLY=false` so
  legacy IPv4 clients keep working through the deprecation window — the eventual IPv4 *removal* is a
  separate, signposted future step, not in this backlog.
- **No invariant moves:** these are addressing/default changes; the data-plane (output clock,
  isolation, last-good-frame) is untouched. Standard guardrails (typing, TDD-first, no IPv4-only
  examples) apply.
