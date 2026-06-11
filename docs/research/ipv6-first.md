# IPv6-first networking — design brief

**Status:** design (2026-06-08). Drives [ADR-0042](../decisions/ADR-0042.md); canonical statement in
[conventions §10](../architecture/conventions.md). Cross-cutting — touches every network-facing
surface. Backlog: [ipv6-first-backlog](../development/ipv6-first-backlog.md).
**Question:** what does **IPv6-first** mean concretely for Multiview — bind/listen defaults, address
& URL handling, SDP, multicast — and what IPv4-only sites in the tree must be remediated so the
product is genuinely IPv6-first (IPv4 legacy-only, on a deprecation path)?

---

## 0. Headline

All network-facing surfaces are **IPv6-first**; IPv4 is **legacy-only and will be removed**. The
posture is IPv6-**first with dual-stack defaults** (so legacy IPv4 peers still interop during the
deprecation window) — **not** IPv6-only yet, and **never** IPv4-first. Concretely: **bind `[::]`
dual-stack** (`IPV6_V6ONLY=false`) not `0.0.0.0`; loopback `[::1]` not `127.0.0.1`; **bracket IPv6
URL literals**; SDP handles **`c=IN IP6`** (no TTL — scope is in the address); IPv6 multicast
**`ff00::/8`** + SSM **`FF3x::/32`** via **MLDv2** is the primary multicast path, IPv4
`239/8`/`232/8` + IGMPv3 the legacy peer. The principle was **undocumented**, so IPv4-only
assumptions accreted; this brief documents it and enumerates the remediation. This is an addressing
posture, not a data-plane change — **no invariant moves** (IPv6 is an address family, not a pacer).

---

## 1. As-built vs missing (verified audit, 2026-06-08)

No convention/ADR/brief stated an IPv6 posture (confirmed: zero IPv6/dual-stack statements in
`conventions.md`/CLAUDE.md). The audit found these IPv4-only / IPv4-centric sites **(as of the
2026-06-08 audit against `origin/main`; some are already being remediated in parallel — where a site
is fixed, the matching `IPV6-*` slice is a no-op/verification-only)**:

| Site | Location | Problem |
|---|---|---|
| **SDP parse** | `multiview-control/src/nmos/is05.rs:340` | `parse_sdp_transport` handles only `c=IN IP4`; `c=IN IP6 …` is silently mangled to `destination_ip = "IP6"`. |
| **SDP generate (WHEP)** | `multiview-preview/src/whep.rs:275/289/306` | Always emits `o=…/c=IN IP4 0.0.0.0`, even when ICE candidates are IPv6 (protocol-incorrect per RFC 8866). |
| **Telemetry binds** | `multiview-telemetry/src/syslog.rs:392`, `snmp.rs:702` | `UdpSocket::bind("0.0.0.0:0")` — IPv4-only. |
| **WHEP native bind** | `multiview-preview/src/whep/native.rs:237` | `bind("127.0.0.1:0")` — IPv4 loopback only. |
| **HA transport bind** | `multiview-engine/src/ha/transport.rs:290` | No IPv6 handling for the cluster socket. |
| **Multicast join** | `multiview-input/src/st2110/transport.rs:104` | Only `join_multicast_v4`; no IPv6 / `MCAST_JOIN_(SOURCE_)GROUP`. |
| **Control listen docs** | `multiview-config/src/lib.rs` | `listen` field documented only with `127.0.0.1`/`0.0.0.0`. |
| **Examples** | `docs/reference/example-streams.md` | All IPv4 (`127.0.0.1`, `239.0.0.1`, …). |
| **Recent docs** | [multicast-transport](multicast-transport.md), [sap-discovery](sap-discovery.md), ADR-0040/0041, backlogs | IPv4-led examples, `c=IN IP4` as the primary SDP form, IPv6 multicast SSM described as "a follow-up" — IPv4-first framing (corrected in the same change as this brief). |

The wire layer is IPv6-capable underneath (Rust `std::net` is dual-family, libav opens bracketed
IPv6 URLs); the gaps are **defaults, examples, and the SDP/multicast handling that hard-codes IPv4**.

---

## 2. What IPv6-first means concretely

- **Bind / listen — dual-stack `[::]` by default.** One IPv6 socket with `IPV6_V6ONLY=false` accepts
  both IPv6 and IPv4-mapped (`::ffff:a.b.c.d`) clients — so the default is IPv6-first **and** still
  serves legacy IPv4 during the transition. Loopback defaults `[::1]`. (Note the platform caveat: on
  some BSDs `IPV6_V6ONLY` defaults to 1; set it explicitly to 0 for dual-stack rather than relying on
  the OS default. Linux defaults to dual-stack via `net.ipv6.bindv6only=0`.) An explicit
  IPv6-only/IPv4-only override is allowed; the *default* is dual-stack.
- **URLs — bracket IPv6 literals.** `udp://@[ff3e::1]:5004`, `rtp://[2001:db8::1]:5006`,
  `srt://[2001:db8::2]:9000`, `[::]:8080` / `[::1]:8080`. Never emit an unbracketed IPv6 literal +
  port. Address parsing accepts both families everywhere (newtypes over `IpAddr`, not `Ipv4Addr`).
- **SDP — `IN IP6` is first-class.** Parse/generate both `c=IN IP4` and `c=IN IP6`. The connection
  line shapes differ (RFC 8866 §5.7): IPv4 multicast is `c=IN IP4 <addr>/<ttl>[/<count>]` (TTL
  mandatory); **IPv6 multicast is `c=IN IP6 <addr>[/<count>]` — no TTL** (the optional slash integer
  is the number of addresses; scope lives in the address). The `o=` origin line addrtype follows the
  family. `a=source-filter` carries IPv6 sources for IPv6 SSM.
- **Multicast — IPv6-first.** IPv6 multicast is `ff00::/8` with a 4-bit flags nibble + 4-bit scope
  nibble (1 iface / 2 link / 4 admin / 5 site / 8 org / e global, RFC 4291); IPv6 SSM is `FF3x::/32`
  (RFC 4607) joined via **MLDv2** (RFC 3810). The IPv4 ranges (`239/8` admin, `232/8` SSM) + IGMPv3
  are the legacy peer. The OS join API is family-agnostic via `MCAST_JOIN_GROUP` /
  `MCAST_JOIN_SOURCE_GROUP` (RFC 3678); a `join_multicast_v6` is required beside the existing v4 one.
- **Docs/config — examples lead IPv6.** Every example address is IPv6 first; an IPv4 form, if shown,
  is labelled *legacy*.

---

## 3. Remediation plan

The audit's sites become the `IPV6-0..` backlog ([ipv6-first-backlog](../development/ipv6-first-backlog.md)),
TDD-first, each with an IPv6 (or dual-stack) test. Priority order: (1) the canonical convention +
this ADR (done in this change); (2) the IPv4-first text in the merged multicast/SAP docs (done here);
(3) the `is05` SDP parser `IN IP6` support + the unified SDP model from [ADR-0041](../decisions/ADR-0041.md);
(4) dual-stack `[::]` binds (control, telemetry, preview, HA); (5) the WHEP answer `IN IP6`; (6) the
IPv6 multicast join path; (7) IPv6-led example configs. None of these change a data-plane invariant.

---

## 4. Test strategy

- **Pure:** address/URL parse+format property tests over both families (bracketed IPv6 literals
  round-trip; reject unbracketed IPv6+port); SDP parse/generate for `IN IP6` incl. the **no-TTL**
  rule (assert an IPv6 `c=` line never emits `/ttl`, and an IPv4 one does); multicast group
  classification across `ff00::/8`/`FF3x::/32` and the IPv4 legacy ranges.
- **Loopback (no special hardware):** bind `[::]`/`[::1]` and connect over both IPv6 and IPv4-mapped;
  IPv6 multicast on `lo`/a dummy interface where supported.
- **Guard:** a check (review + ideally a lint/grep gate) that new code does not introduce
  `0.0.0.0`/`127.0.0.1`/`Ipv4Addr`-only binds or `IN IP4`-only SDP — flagging the IPv4-only pattern.

---

## 5. Citations

- RFC 4291 (IPv6 addressing — multicast `ff00::/8`, scope nibble), RFC 4607 (SSM — IPv4 `232/8` +
  IPv6 `FF3x::/32`), RFC 3810 (MLDv2), RFC 3678 (the family-agnostic `MCAST_JOIN_(SOURCE_)GROUP`
  socket API), RFC 8866 (SDP — `c=` `IN IP4 addr/ttl[/n]` vs `IN IP6 addr[/n]`, §5.7), RFC 3493
  (basic socket API for IPv6, `IPV6_V6ONLY`).
- In-repo: [conventions §10](../architecture/conventions.md), [ADR-0042](../decisions/ADR-0042.md),
  [ADR-0040](../decisions/ADR-0040.md)/[ADR-0041](../decisions/ADR-0041.md) (multicast/SAP);
  `is05.rs:340`, `whep.rs:275`, `whep/native.rs:237`, `syslog.rs:392`, `snmp.rs:702`,
  `ha/transport.rs:290`, `st2110/transport.rs:104`, `multiview-config/src/lib.rs`,
  `docs/reference/example-streams.md`.
