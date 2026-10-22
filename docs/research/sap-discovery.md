# SAP (Session Announcement Protocol) discovery + announcement — design brief

**Status:** design (2026-06-08). Drives [ADR-0041](../decisions/ADR-0041.md). Sits **on top of**
the IP-multicast transport ([multicast-transport](multicast-transport.md), ADR-0040 — SAP
discovers/announces, multicast carries). **Unifies** the planned AES67 SAP/SDP
([aes67-delivery](aes67-delivery.md) §6, [ADR-0033](../decisions/ADR-0033.md); backlog AES67-5/-6)
into thin profiles. **SAP here = Session Announcement Protocol (the protocol VLC surfaces as
"Network Streams (SAP)"), not anything to do with subtitles.**

> **IPv6-first ([ADR-0042](../decisions/ADR-0042.md) / [conventions §10](../architecture/conventions.md)).**
> SAP is IPv6-first: the listener joins the IPv6 SAP group `FF0X::2:7FFE` (and the IPv4 groups as the
> **legacy** peer), the SDP model treats **`c=IN IP6` as a first-class form** (IPv6 multicast carries
> **no TTL** — the slash is an address *count*, RFC 8866 §5.7; only IPv4 takes `/ttl`), derived MRLs
> bracket IPv6 literals (`udp://@[ff3e::1]:p`, `rtp://[…]`), and announced outputs lead with IPv6.
> Where SDP examples below show `IN IP4` they are the legacy form.

**Question:** what is the implementation-ready architecture for **SAP discovery and announcement —
listen for, and emit, the SDP descriptions of multicast media sessions** — so Multiview is
plug-and-play discoverable by, and can discover, VLC / Dante-AES67 / RAVENNA gear, without a second
SAP/SDP stack and without ever back-pressuring the engine?

> **Scope note.** SAP is **discovery + announcement only** — it carries an SDP descriptor; the
> media itself rides RTP or raw UDP MPEG-TS over multicast, which the multicast transport
> ([ADR-0040](../decisions/ADR-0040.md)) opens. This brief does **not** re-specify the media path
> — it adds the SAP packet engine, the general SDP model, the discovered-session inventory, the
> announcer, and the AES67 unification, all on the control/discovery plane.

---

## 0. Headline — SAP is discovery + announcement, not transport

SAP (RFC 2974, Experimental) periodically **multicasts an SDP** describing a media session to a
well-known group on **UDP 9875**; receivers build a browsable list (VLC's "Network Streams (SAP)").
SAP carries **only** the SDP descriptor — the media rides RTP or raw UDP MPEG-TS on the address/port
in the SDP `c=`/`m=` lines, which the multicast transport (ADR-0040) opens. Multiview wants both
halves: **(A) SAP receive** — join the group set, parse SAP/SDP into a **bounded, untrusted**
discovered-session **inventory** surfaced to the operator/WebUI like VLC's list, which the operator
**binds to a Source** (never auto-ingest); **(B) SAP announce** — periodically emit SDP for
Multiview's own multicast outputs so VLC/Dante/RAVENNA discover them.

The load-bearing decision: build **one** essence-agnostic SAP engine + **one** general SDP model,
and make the already-planned AES67 SAP/SDP (AES67-5/-6, [ADR-0033](../decisions/ADR-0033.md)) thin
**profiles/consumers** — **not a second SAP**. The whole subsystem is pure-Rust big-endian byte work
+ a thin SDP serializer, off-by-default behind the existing **`st2110`** feature (no FFI,
`unsafe_code = forbid`, LGPL-clean), and lives strictly on the **control/discovery plane** —
physically incapable of back-pressuring the engine (inv #1/#10).

---

## 1. As-built vs missing (verified against the tree)

**As-built:** **no SAP code exists** (zero hits for `9875`/RFC 2974/`sap`). Multicast TS media rides
`SourceKind::Ts { url }` (`schema.rs:213`; `pipeline.rs:3544` routes `Ts` → `SourceLocation::Url`,
live) — a `udp://@group:port` string handed to libav's avio, which does the IGMP join transparently
(`demux.rs`). `pipeline.rs:4275` confirms "no `rtp://` 32-bit source kind exists in the CLI path
yet, so `Rtp32` is unused." `Output` (`schema.rs:531`) has no `Udp`/multicast variant;
`PushProtocol::UdpTs` exists (`sink.rs:1028`) but is unreachable (`build_outputs`, `pipeline.rs:3203`,
only routes `Rtmp`/`Srt`). Both media gaps are owned by the multicast transport (ADR-0040 / MC-3,
MC-4) — SAP **depends** on them, it does not re-solve them.

Two narrow SDP parsers exist **to generalize**: `nmos/is05.rs:340` `parse_sdp_transport` (`c=` +
`m=video <port> RTP/AVP <fmt>` + `a=source-filter` only — no `m=audio`, no `rtpmap`, no generator;
its doc-comment says "not a full RFC 8866 SDP implementation"); and `st2110/sdp.rs`'s `parse_rtpmap`
(the AES67 `a=rtpmap` parser — PT/name/clock/channels). The **NMOS IS-04/IS-05** node
(`nmos/mod.rs` `NmosRegistry`) is the richer, trustworthy **parallel** discovery already built. The
wait-free publish primitive `LatestState`/`ArcSwapOption` (`isolation.rs`) and the drop-oldest
`Event::HealthWarningRaised` path are reusable.

**Missing (this brief ships):** the entire RFC 2974 SAP packet engine; the general SDP model; the
multicast receive socket + listener + bounded session table + purge; the discovered-session
inventory + control endpoint + bind-to-Source; the SAP announcer + SDP generator; and the AES67-5/-6
convergence onto the shared layer.

---

## 2. Scoping — SAP vs NMOS vs mDNS/DNS-SD

SAP is the **simple, unauthenticated, VLC- and Dante-AES67-compatible** LAN multicast
announce/discover protocol — soft-state, periodically re-announced, **spoofable** by any host on the
scope. **NMOS IS-04/IS-05** (already built here) is the modern, trustworthy, registry-based discovery
over DNS-SD with staged/active + scheduled-TAI activation — the broadcast-facility path. **mDNS/DNS-SD**
(Bonjour) is peer-to-peer service discovery (and is the transport NMOS IS-04 rides). **RIST** is a
reliable *transport*, not discovery — out of scope. Position SAP as **complementary** to NMOS, never
a replacement and **never auto-bridged** into the NMOS registry (NMOS specs do not mention SAP; they
are separate trust worlds). The inventory presents SAP sessions as a **visually/semantically distinct,
lower-trust** list from NMOS resources; the discovery endpoint lists both, clearly labelled by source.

---

## 3. The SAP packet + lifecycle (RFC 2974, verified)

**Wire format** (network byte order; verified against RFC 2974 §6 + VLC `sap.c`): one **flags byte**
packs all six fields MSB-first — **V (top 3 bits**, value MUST be 1; verify `flags >> 5 == 1`), **A**
(`0x10`: 0 = IPv4 32-bit source / 1 = IPv6 128-bit source), **R** (`0x08` reserved, set 0 / ignore),
**T** (`0x04`: 0 = announce / 1 = delete), **E** (`0x02` encrypted), **C** (`0x01` zlib-compressed).
Then an **auth-length byte** = number of **32-bit WORDS** of auth data (**skip `auth_len*4` bytes** —
a VLC bug does `buf += auth_len` treating it as bytes; a compliant parser MUST skip `auth_len*4` and a
**regression test MUST assert we do not replicate VLC's bug**; masked in practice because announcers
emit `auth_len = 0`). Then a 16-bit **message-id hash**, the **originating source** (4 or 16 bytes per
A), optional auth data, an optional **payload-type** field (`"application/sdp\0"` — but the default is
to **omit both the string and its NUL**, and the receiver detects SDP because the body begins with
`v=0`), then the **SDP**.

**Lifecycle.** A session is keyed by **(msg-id hash, originating source)**; the hash MUST stay
**stable** across periodic refreshes and **change** when the SDP changes; **never emit hash 0**.
Announce cadence is bandwidth-limited `interval = max(300 s, 8·N·ad_size / 4000 bps)` with jitter
`offset = rand(interval·2/3) − interval/3`, i.e. **±1/3 of the interval** (the "2/3" is the *window
width*, not the bound — the common "±2/3" is a misread; verified). Receivers **purge** implicitly
after **`max(10×observed-period, 1 hour)`** of silence; a `T=1` **deletion** carries the `o=` origin
only and is a **hijack vector** — VLC ignores inbound deletions and so do we (we MAY send one on
graceful teardown; we NEVER honour an inbound one against an operator-confirmed input). **Reject
`E=1`** (no algorithm in SAPv2; VLC rejects). Support `C=1` zlib-inflate **with a hard
decompressed-size cap** (inflation DoS).

---

## 4. Groups + scoping (verified)

All SAP traffic uses **UDP 9875**, TTL SHOULD be 255; scope is expressed by **address choice**
(RFC 2365 admin scoping), **not by TTL** (RFC 2974 discourages TTL scoping). IPv4 groups (verified
against VLC + RFC 2365/2974):

- **224.2.127.254** — global (informally "sap.mcast.net" — but that DNS label is **not** in the
  RFC/IANA, only a convention; cite the address). The IPv4 global-scope session range is
  `224.2.128.0–224.2.255.255`.
- **239.195.255.255** — org-local (top of `239.192.0.0/14` — `.195`, not `.192`).
- **239.255.255.255** — local (top of `239.255.0.0/16`); the **de-facto AES67/Dante/RAVENNA** group
  shipping gear actually uses — **standards-sanctioned** (it is the highest address of the RFC 2365
  IPv4 local scope, which RFC 2974 §3 designates as the SAP group for that scope), not a
  non-standard choice.
- **224.0.0.255** — link-local. IPv6 = `FF0X::2:7FFE` (X = scope nibble).
- **Footgun:** `224.2.127.255` is obsolete SAPv0 — MUST NOT use.

**Receive:** join the **full set** `{224.2.127.254, 239.195.255.255, 239.255.255.255, 224.0.0.255}` +
IPv6 `ff0X::2:7ffe` to discover the most sessions (joining only the RFC global group silently misses
every AES67/Dante session). **Announce:** pick the SAP group from the **scope of the media address**
actually streamed (`239.255/16` → `239.255.255.255`; `239.192/14` → `239.195.255.255`; `224.0.0/24` →
`224.0.0.255`; else → `224.2.127.254`); hard-coding the global group for a site-local stream is the
#1 "VLC shows nothing" bug. Model the **SAP group** and the **SDP `c=` media group** as two distinct
typed fields — they are different addresses.

---

## 5. The general SDP model

**One** essence-agnostic SDP model in `multiview-input/src/sdp/` (RFC 8866 line syntax, wire-compatible
with the RFC 4566 reference): session-level `v=0` / `o=<user> <id> <ver> IN IP6 <host>` (IPv6-first;
`IN IP4` is the legacy form) / `s=<name>`
(the **only** human label VLC shows — must be set, unique per announced output) / optional `i=` /
`t=0 0` (permanent) / a connection line that is **IPv6-first**: `c=IN IP6 <group>[/<count>]`
(**no TTL** — scope is in the IPv6 address; the slash integer is an address *count*) is the primary
form, with the **legacy** IPv4 form `c=IN IP4 <group>/<ttl>[/<count>]` (**TTL mandatory** for IPv4
multicast; order is **group/TTL/count** — the number after the first slash is the **TTL, not a CIDR
prefix** — RFC 8866 §5.7); strip
`/ttl[/count]` before forming the join URL; be liberal accepting odd TTLs). Per-media
`m=<media> <port>[/<count>] <proto> <fmt>` with typed `proto` (`RTP/AVP` | `udp` | other),
`a=rtpmap:<pt> <name>/<clock>[/<ch>]`, `a=source-filter` (RFC 4570 incl/excl for SSM), `a=ptime`,
`a=type:broadcast`, `a=recvonly`, `b=RR:0`.

**The proto token, not the PT number, is the framing discriminator:** `RTP/AVP` ⇒ strip the 12-byte
RTP header per datagram; `udp` ⇒ bare 188-byte TS packets. (`m=video <port> udp 33` is a real-world
**convention** — `udp` is a valid RFC 4566 proto but does not itself define the `fmt` meaning, so the
`33` is reused by convention, not normatively; there is **no** `MP2T/AVP` proto token.) **PT 33 =
MP2T/90000 is static** (RFC 3551), so the `rtpmap` is redundant — a robust receiver handles PT 33
**even when `a=rtpmap` is absent**.

This model **generalizes** the two narrow parsers: `nmos/is05.rs:340` (video-only; IS-05
`transport_file` delegates here) and `st2110/sdp.rs`'s `parse_rtpmap` `a=rtpmap` pattern (promoted
into the shared model). It carries the AES67 audio-profile extras (`ts-refclk`/`mediaclk`/`ptime`) as
**typed-but-optional** attributes the audio profile validates.

---

## 6. Receive → inventory → bind architecture

A SAP **listener** (supervised tokio task, off the hot path, per-interface UDP sockets joined to the
group set on 9875) recvs into a **bounded drop-oldest** buffer, parses each packet with the §3 codec
(reject `E`, reject hash 0, bounds-check `auth_len*4`, zlib-cap), parses the SDP with the §5 model,
and upserts into a **fixed-capacity** session table keyed on (hash, source) with **per-source + global
parse rate limits** and implicit `max(10×period, 1 h)` purge. The listener publishes a newest-wins
`SapInventory` snapshot via the engine `LatestState`/`ArcSwapOption` pattern (`isolation.rs` —
wait-free single-atomic-store publish, no lock, no `.await`) so the control plane reads it on its own
schedule and a stalled UI never blocks the listener.

The control plane exposes a **read-only `GET /api/v1/discovery`** (this brief; unified with AES67-14
to list SAP **+** NMOS, labelled by trust) returning each session's `s=` name, `i=` description,
**derived MRL** (proto → scheme: `udp` → `udp://@group:port`, `RTP/AVP` → `rtp://@group:port`; group
from `c=` minus `/ttl`, port from `m=`; source from `a=source-filter` for SSM), and `trust =
untrusted`. **Bind:** the operator picks a session and the control plane **materializes** a
`SourceKind` — today the raw-UDP-TS case maps to `SourceKind::Ts { url: "udp://@g:p" }` (or the typed
`SourceKind::Udp` once MC-3 lands); RTP-framed-non-TS sessions are surfaced **"unbindable"** until the
multicast RTP ingest kind exists (ADR-0040 names this gap). A SAP-discovered Source is just another
**sampled** input feeding the last-good-frame store (inv #2), never a pacer; binding it to a tile is
**Class-1** live-apply (inv #11). A pure classify-style selector answers "which tile(s) can bind this
SDP?" without reaching into the engine.

---

## 7. Announce-for-outputs architecture

When Multiview serves a multicast RTP-TS or raw-UDP-TS output, a SAP **announcer** (independent tokio
timer task, **never** the per-tick output loop) generates the SDP from the **authoritative output
config** (so `c=`/`m=`/`rtpmap` stay in lockstep with what is actually transmitted), wraps it in a
SAP packet (flags `0x20`, `|0x10` for an IPv6 origin; `auth_len = 0`; a **stable non-zero per-output
hash** derived from the output id; the originating source; `"application/sdp\0"` prefix for VLC
clarity), and sends to the SAP group chosen by the §4 scope selector on 9875 TTL 255. SDP emitted:
`v=0` / `o=- <id> <ver> IN IP6 <self>` / `s=<program name>` / `t=0 0` / `c=IN IP6 <group>` (IPv6-first,
no TTL; the **legacy** IPv4 form is `o=… IN IP4 <self>` / `c=IN IP4 <group>/<ttl>`) /
`m=video <port> RTP/AVP 33` + `a=rtpmap:33 MP2T/90000` (RTP-TS) **or** `m=video <port> udp 33`
(raw-UDP-TS) / `a=type:broadcast` / `a=recvonly` / `a=tool:multiview` / `b=RR:0`. Re-announce on a
deliberate timer (default **≥30 s** — interoperable with VLC/AES67; the RFC bandwidth-fair
`max(300 s, …)` timer is available as an option for many-session scale). Bump the hash + `o=`
sess-version on any SDP change. On graceful teardown / Class-2 make-before-break migration, send a
**courtesy `T=1` deletion** (documenting that VLC ignores it and relies on timeout).

**Critical dependency:** announce-for-outputs requires a multicast egress to announce — `Output::Udp`
/ `PushProtocol::UdpTs` (ADR-0040 / **MC-4**). The announcer **ships with** that egress (no
announce-without-a-stream-to-announce; no-partial rule).

---

## 8. The AES67 unification (consumer/profile, no second SAP)

Extract the **general SAP engine** to `multiview-input/src/sap/` and the **general SDP model** to
`multiview-input/src/sdp/` (both under the existing **`st2110`** feature). The AES67 plan's
`st2110/sdp.rs` becomes a thin **`Aes67Profile`** that validates/extracts L16|L24 + `ptime` (Class A
1 ms) + `ts-refclk:ptp=GMID:domain` + `mediaclk:direct=offset` **atop** the general SDP `Media`
(delegating parse/generate to `sdp/`); `st2110/sap.rs` evaporates into "instrument the general `sap/`
engine with the AES67 default-announce-group `239.255.255.255` + ≥30 s cadence". The
`Aes67AudioProducer` (AES67-3) **filters** the general SAP inventory to AES67 sessions and reads the
media clock at 48 kHz.

**ADR-0041 supersedes the audio-only placement clause of [ADR-0033](../decisions/ADR-0033.md) §6/§9**
and re-points AES67-5/-6 in the AES67 backlog at the shared modules (one-line edits, no behaviour
change — both still ship under `st2110`, tested together). This guarantees a single RFC 2974 codec and
a single RFC 8866 model serve **video + audio + MP2T**, satisfying the no-second-SAP mandate. Decision
sequencing: if this brief leads, AES67-7 (SAP convergence) is an `S`; if the AES67 work lands first
under `st2110/`, that slice grows to a real refactor (`M`) — so **land SAP's `sap/`+`sdp/` extraction
before, or jointly with, AES67-5/-6**.

---

## 9. Security + isolation (inv #1 / #10 / #11)

SAP announcements are **unauthenticated and trivially spoofable** (RFC 2974 §10 documents spoofing +
local-flood DoS; the PGP/CMS auth header and the `E` flag are **optional and unimplemented in
practice** — VLC skips auth without checking and rejects encrypted; they are *optional*, not formally
*deprecated*, and RFC 2974 is Experimental, not obsoleted). Therefore discovered sessions are
**untrusted hints**: surface as candidates, require **explicit operator confirm-to-bind**, **never
auto-ingest** (auto-joining a SAP-announced address would let any LAN host steer the engine to join
arbitrary multicast groups — a join-amplification / resource-exhaustion vector).

The session table is **adversary-controlled**, so enforce hard bounds Multiview-side (SAP gives none):
a **fixed-capacity** table (evict oldest on overflow), a **bounded** socket recv (drop on overflow),
**per-source + global parse rate limits**, and a **zlib decompressed-size cap** before allocating.

**Isolation (inv #1/#10):** the listener, the announcer timer, and the inventory publish are all
**off the output-clock data plane** — supervised tokio tasks that publish via `LatestState`
(wait-free) and feed the control plane via bounded drop-oldest; the engine **never awaits** them, and
a SAP flood or a giant SDP must never allocate unboundedly or back-pressure decode→composite→encode.
Reuse the bounded drop-oldest receive seam (the `channel_bridge`; the freshness-fix the AES67 backlog
plans as AES67-4 applies — on a full channel today the *newest* unit is dropped, a true drop-oldest
ring is preferable for a jitter buffer). Binding a confirmed session to a tile is **Class-1** (inv
#11). Expire by timeout, never by a spoofable inbound `T=1`; never let discovery state delete an
operator-confirmed running input.

---

## 10. Crate / feature / config surface

**Placement:** new `multiview-input/src/sap/` (`packet`, `groups`, `session_table`, `listener`,
`announcer`) + new `multiview-input/src/sdp/` (the general model), both gated by the existing
**`st2110`** feature (`= [tokio/net]`, no FFI, `unsafe_code = forbid`). The discovery endpoint extends
the `multiview-control` NMOS discovery surface. **Config** (`schema.rs`, internally tagged
`#[serde(tag = "kind")]` `#[non_exhaustive]`, **never `untagged`**): a SAP discovery capability
(listen group set + interfaces, defaulting to the four IPv4 groups + IPv6, **opt-in** — join no groups
until the operator enables discovery, given the untrusted nature) and a per-output `announce_via_sap`
+ name/group/ttl/interval.

**The two media gaps are owned by ADR-0040 (multicast), not re-solved here** — SAP declares the
cross-deps: **bind-to-Source** consumes **MC-3** (raw-UDP-TS ingest works today via `Ts{url}`;
RTP-framed ingest is the ADR-0040 gap, surfaced "unbindable"); **announce** consumes **MC-4**
(`Output::Udp` egress).

---

## 11. Test strategy (no special hardware — loopback multicast on lo)

All tests are NIC-free or use loopback multicast on `lo` — no AES67/Dante hardware needed.

- **Packet codec** property tests: round-trip encode→decode over all flag combos; **explicit
  regression that `auth_len` is skipped as WORDS×4** (not VLC's bytes bug); hash 0 rejected; `E=1`
  rejected; `C=1` inflate honoured with a size cap; bounds-checks never index out of range (no
  `indexing_slicing` — `get()`/`try_into()` with length checks).
- **SDP model** property tests: round-trip parse→generate; `c=` group/TTL/count order; proto→scheme
  mapping (`udp` → `udp://@`, `RTP/AVP` → `rtp://@`) with `/ttl` stripped; **PT 33 handled with AND
  without `a=rtpmap`**; `source-filter` → SSM (S,G); the generalized parser still satisfies the
  `is05.rs:603`/`:613` video cases and the AES67 audio-profile cases.
- **Session table:** implicit purge at `10×period`; fixed-capacity eviction under flood; per-source
  rate limit.
- **Loopback interop** (gated `#[ignore]`/feature): announce our SDP to a local multicast group on
  `lo` and assert our own listener discovers it with the right name/group/port/scheme (closes the
  loop without VLC); optionally drive headless VLC announcing `#rtp{mux=ts,sap,…}` and assert
  discovery.
- **Isolation/security chaos gate (inv #1/#10):** flood the SAP socket + wedge the inventory consumer;
  assert bounded memory, the table never grows past cap, and **zero** engine back-pressure / the
  output clock keeps ticking (template: the AES67-13 `drain_is_bounded_and_never_awaits` /
  `channel_bridge` wedge test). Validate emitted SDP ordering with `ffprobe`.

---

## 12. Citations

- **SAP / SDP / RTP:** RFC 2974 (SAP — wire format §6, groups/port §3, interval §3.1, purge §4,
  security §10; Experimental), RFC 8866 (SDP, obsoletes RFC 4566; `c=` `/ttl[/count]`), RFC 3551
  (RTP PT 33 = MP2T/90000 static), RFC 2250 (MPEG-2 TS in RTP), RFC 4570 (`a=source-filter`), RFC 2365
  (admin scoping — local `239.255/16` → `239.255.255.255`, org `239.192/14` → `239.195.255.255`),
  RFC 3376/3810/4604/4607 (IGMPv3/MLDv2/SSM).
- **VLC interop contract:** `modules/services_discovery/sap.c` (group set, `flags >> 5`, the
  auth-skip bug, `T=1` ignored, hash ≠ 0) and `src/stream_output/sap.c` (announcer scope selector,
  flags `0x20`/`0x30`, `IPPORT_SAP 9875`).
- **AES67 / Dante:** AES67-2018 (lists discovery options, mandates none — the ~30 s SAP cadence is an
  **Audinate/Dante implementation choice**, not an AES67 requirement; `239.255.255.255` is the de-facto
  group). IANA multicast registry (`224.2.127.254` SAPv1; no "sap.mcast.net" label).
- **In-repo:** [multicast-transport](multicast-transport.md) + [ADR-0040](../decisions/ADR-0040.md)
  (the media path + MC-3/MC-4 deps), [aes67-delivery](aes67-delivery.md) §6/§6.1/§7 +
  [aes67-backlog](../development/aes67-backlog.md) AES67-4/-5/-6/-13 + [ADR-0033](../decisions/ADR-0033.md)
  (the AES67 plan this unifies), `conventions.md` §5 (invariants); `is05.rs:340`, `st2110/sdp.rs` `parse_rtpmap`,
  `transport.rs:149/171/297/410`, `isolation.rs`, `schema.rs:213/531`, `sink.rs:1028`,
  `pipeline.rs:3203/3544/4275`, `event.rs:284/307`.
