# IP multicast transport (UDP-TS + RTP) — input + output — design brief

**Status:** design (2026-06-08). Drives [ADR-0040](../decisions/ADR-0040.md). The media-path
foundation the **SAP** discovery layer ([sap-discovery](sap-discovery.md), ADR-0041) and
AES67/ST 2110 sit **on top of** (SAP discovers; multicast carries). Consistent with
[ADR-0031](../decisions/ADR-0031.md) (LGPL FFmpeg already enables `udp,rtp`),
[ADR-0035](../decisions/ADR-0035.md) (the sense→detect→warn `HealthWarning` precedent),
[ADR-0030](../decisions/ADR-0030.md) (`AVIOInterruptCB`), and **[ADR-0042](../decisions/ADR-0042.md) /
[conventions §10](../architecture/conventions.md) — IPv6-first.**

> **IPv6-first ([ADR-0042](../decisions/ADR-0042.md)).** This subsystem is **IPv6-first**: the
> primary multicast path is IPv6 (`ff00::/8`, SSM `FF3x::/32` via MLDv2), addresses bind/derive IPv6
> (bracketed literals, e.g. `udp://@[ff3e::1]:5004`), and the IPv4 ranges (`239/8`/`232/8` + IGMPv3)
> are the **legacy** peer. Where an example below shows an IPv4 address it is the legacy form; the
> IPv6 form is primary. `MulticastGroup` is `IpAddr` (v6 or v4); the IPv6 join path
> (`join_multicast_v6` / family-agnostic `MCAST_JOIN_SOURCE_GROUP`) is **required, not a follow-up**.
> IPv6 multicast `c=` lines carry **no TTL** (scope is in the address) — `ttl` is an IPv4-only knob.

**Question:** what is the implementation-ready architecture for **IP multicast as a
first-class media transport — UDP-MPEG-TS and RTP/MP2T over IPv4/IPv6 multicast, ingest *and*
egress** — and how does the engine **warn, bulletproofly and without false positives, when it
is running in a container/devcontainer without host networking** (where multicast silently
cannot work)?

> **Scope boundary (load-bearing).** This subsystem owns **compressed contribution/distribution
> multicast** (H.264/HEVC/MPEG-2 in TS-over-UDP or TS-over-RTP, MP2T PT 33), opened via libav
> and free-running on the output clock like every other source. **Uncompressed SMPTE ST 2110-20**
> (RFC 4175, multi-Gbps, hard PTP/ST 2059 lock, jumbo frames, kernel-bypass) is **out of scope**
> — a separate heavy effort. We **reuse only** the existing `st2110` module's RTP-header parser,
> RFC-1982 sequence math, ST 2022-7 `HitlessReconstructor`, and bounded drop-oldest channel
> pattern — never the -20 raster assembly or the PTP lock.

---

## 0. Headline

IP multicast — UDP-MPEG-TS and RTP-over-multicast (IPv4/IPv6), **ingest and egress** — becomes
a first-class, **typed** Multiview transport. Today it can only ride
`SourceKind::Ts { url: "udp://@group:port" }` **untyped** through libav, there is **no
`Output::Udp`** so the already-implemented `PushProtocol::UdpTs` (`sink.rs:1028`) is
**unreachable**, and a multicast configured inside a default-bridge container/devcontainer
fails **silently** — the IGMP join succeeds at the socket layer and **zero packets ever
arrive**. This brief specifies: **(a)** typed `SourceKind::Udp`/`Rtp` ingest and
`Output::Udp`/`Rtp` egress with structured multicast fields
(group/port/transport/sources/block/interface/ttl/buffer/fifo/overrun), mirroring the
**`RtspOptions`** typed-extension precedent (`schema.rs:175`/`:245`) — not a raw URL; **(b)** a
deterministic FFmpeg-`udp`/`rtp` URL-from-typed-config derivation with the SSM/interface/buffer
reliability defaults **baked in**; **(c)** the **container-without-host-networking
`HealthWarning`** (the operator's explicit ask), bulletproof and false-positive-free, modelled
on [ADR-0035](../decisions/ADR-0035.md) sense→detect→warn.

Every invariant holds: a multicast receive socket is an **input jitter buffer, never a pacer**
(inv #1); a never-arriving/dropping multicast rides LIVE→STALE→RECONNECTING→NO_SIGNAL (inv #2);
the TS 33-bit PTS is unwrapped and the output re-stamps from the tick (inv #1/#3/#6); the canvas
is encoded once and the same packets fan to the multicast sink (inv #7); both the receive socket
and the egress sink are bounded drop-oldest and the engine never awaits them (inv #10).

---

## 1. As-built vs missing (verified against the tree)

**As-built (verified this session):**

| Layer | Location | State |
|---|---|---|
| Ingest kinds | `multiview-config/src/schema.rs:213` | `SourceKind` = Bars/Solid/Clock/Rtsp/Hls/Youtube/Ts/Srt/Rtmp/Ndi/File. **No udp/rtp/multicast variant.** The only multicast carrier is `Ts { url }` (`schema.rs:264`, just a URL). |
| Ingest open | `pipeline.rs:3519`/`:4219` | `ingest_plan_for` maps `Ts` → `SourceLocation::Url` regardless of scheme; `open_and_stream` passes the URL to `input_with_dictionary`. `udp://@group:port` opens via libav today — **untyped, no interface/buffer/SSM control, no validation**. |
| PTS wrap | `pipeline.rs:4268`/`:4275` | `PtsNormalizer` hardcoded `WrapBits::Mpeg33`; comment: "no raw `rtp://` 32-bit source kind exists in the CLI path yet, so `Rtp32` is unused." `Rtp32` (`normalize.rs`) is reserved for a future raw-essence path. |
| Multicast socket | `st2110/transport.rs:104` | Only `RtpReceiver::join_multicast_v4(group, iface)` — ASM, IPv4, **feature-gated `st2110`**, sets no SO_RCVBUF. No IGMP/interface/buffer code in the CLI path. |
| Egress kinds | `schema.rs:531` | `Output` = RtspServer/LlHls/Hls/Ndi/Rtmp/Srt. **No `Output::Udp`/`Rtp`.** |
| Push protocol | `sink.rs:1020`/`:1037` | `PushProtocol` = Rtmp/Srt/Rtsp/**UdpTs**; `muxer_name` maps `Srt\|UdpTs => "mpegts"`. But `build_outputs` (`pipeline.rs:3203`) only builds `Rtmp`/`Srt` — **`UdpTs` is unreachable from config**, and there is **no `PushProtocol::Rtp`**. |
| Egress isolation | `pipeline.rs:138` | `SINK_WEDGE_GRACE = 2 s` bounds the per-packet fan-out + per-sink join, so a stalled sink is detached, never back-pressures the clock (inv #1/#10). |
| RTP prior art | `st2110/{rtp,transport,st2022_7}.rs` | Pure `RtpPacket::parse`, RFC-1982 `seq_distance`/`seq_after`, bounded drop-oldest `channel_bridge`/`ChannelPacketSource` (`transport.rs:149`/`:410`), `PacketSource::poll_packet` (`:297`), essence-agnostic `HitlessReconstructor`/`DualPathPacketSource` (ST 2022-7). Parsers compile in the **default** build; only the socket layer is `#[cfg(feature="st2110")]`. |
| Resilience | `pipeline.rs:3837`/`:3762` | `reconnect_backoff` (capped-exponential + jitter); `PRIME_WAIT_BUDGET` (1.5 s) initial-frame grace; lock-free `TileStore` sampled by the clock (inv #1/#2). |
| Warning machinery | `multiview-events/src/event.rs:284`/`:307` | `WarningCode` carries **only** `GpuPresentNoVulkanAdapter` today; `HealthWarning { code, severity, subsystem, message, remediation, since, active }`; `Event::HealthWarningRaised`/`Cleared` on `Topic::Alerts`. Emit precedent: `emit_capability_warnings` + `CompositeMismatchView` (`warning_ingest.rs:38`/`:60`) → `publisher.publish_event` (drop-oldest, inv #10); CLI build-site `capability_warn::probe_and_emit` (`capability_warn.rs:111`, called `main.rs:269`); `InMemoryWarningStore` keys by `WarningCode.as_str()`; `GET /api/v1/health` (`routes/health.rs`). |

**Honesty correction.** An earlier framing assumed a typed `SrtOptions` had "merged" as the
model to mirror — it has **not**: the merged SRT change ([ADR-0039](../decisions/ADR-0039.md)) is
**docs-only**; `SrtOptions` is backlog item SRT-0, not code. The **implemented** precedent is
**`RtspOptions`** on `SourceKind::Rtsp` (`schema.rs:175`/`:245`) — an `Option<Opts>` beside the
`url`, default-skipped, internally tagged. That is the shape we mirror.

**Missing (the work):** typed `SourceKind::Udp`/`Rtp` + `UdpOptions`/`RtpOptions`; the libav
URL-from-typed-config derivation; `Output::Udp`/`Rtp` reaching `PushProtocol::UdpTs` + a new
`PushProtocol::Rtp`; address-range validation; SO_RCVBUF readback warning; multicast
socket/buffer/drop telemetry; the `WarningCode::ContainerNoHostNetworking` detector + emit + the
`MulticastJoinedNoTraffic` runtime confirmation; CI loopback + the isolation chaos gate.

---

## 2. IP multicast model — ASM/SSM, IGMP/MLD, ranges, scoping

**Address ranges (RFC 5771 / IANA — verified).** IPv4 multicast = **224.0.0.0/4**. Blocks
Multiview must classify: **224.0.0.0/24** Local Network Control — link-local, **never forwarded
off-segment regardless of TTL** (RFC 1112/5771) → **reject** as a media group; **232.0.0.0/8**
Source-Specific Multicast (RFC 4607) → **require a source list** (`232.0.0.0` itself reserved);
**233.0.0.0/8** GLOP; **239.0.0.0/8** administratively scoped (RFC 2365) — locally assigned, not
globally unique (the RFC-1918 analogue, the right home for in-plant media), within it
**239.255.0.0/16** Local Scope and **239.192.0.0/14** Organization-Local. IPv6 = **ff00::/8**
with a flags nibble (T=0 well-known / T=1 transient) and a scope nibble (1 iface / 2 link /
4 admin / 5 site / 8 org / e global) (RFC 4291); IPv6 SSM = **FF3x::/32** (`FF3x::4000:0000`
reserved).

**ASM vs SSM.** Any-Source Multicast is a `(*,G)` join (any sender) — IGMPv1 (RFC 1112) /
IGMPv2 (RFC 2236, explicit Leave) on IPv4, MLDv1 (RFC 2710) on IPv6. Source-Specific Multicast
is a `(S,G)` "channel" join (RFC 4607) — distinct senders to the same `232.x` group are distinct
channels (collision-free reuse), the modern broadcast-aligned choice; it **requires** per-source
INCLUDE/EXCLUDE filtering, i.e. **IGMPv3 (RFC 3376)** on IPv4 / **MLDv2 (RFC 3810)** on IPv6
(RFC 4604/4607 — verified). SSM is the recommended ingest default where the sender IP is known;
ASM `239/8` is the LAN default where SSM isn't available.

**TTL scoping ≠ admin scoping.** TTL bounds router hops; admin scoping (`239/8`) bounds at
configured boundaries and is not globally unique. libav's egress multicast TTL **defaults to 16**
(not 1) — for a contained plant set `ttl=1` explicitly; never carry media in `224.0.0.0/24`
(TTL is ignored there).

**Deployment prerequisites Multiview cannot fix in code** (surfaced as docs + the runtime
no-traffic diagnostic, not a software feature): an IGMP/MLD **querier** must exist on an
L2-snooping segment or membership reports are pruned and no traffic flows; PIM/L3 multicast
routing is a separate WAN concern, out of scope for a single-segment LAN multiview; **cloud VPCs
(AWS/Azure/GCP) carry no multicast at all**.

---

## 3. FFmpeg `udp`/`rtp` option mapping → typed config

We **derive** the libav URL deterministically from typed config (single validated source of
truth, defaults guaranteed) rather than let an operator hand-craft it. **The `@` convention**
(verified-nuanced): `udp://@group:port` forces receive/listen semantics — `@` is the generic URL
userinfo separator stripped by `av_url_split`, **not documented FFmpeg multicast syntax** nor
handled inside `udp.c`; with empty userinfo `udp.c` joins via `IP_ADD_MEMBERSHIP` in read mode.
So we build the URL **with `@` for ingest, without for egress**, deterministically — never by
string-templating operator fields.

**UDP options (`libavformat/udp.c` — verified names/defaults):**

- `buffer_size` (bytes, `SO_RCVBUF`) default → built-in **384 KiB** input / 32 KiB output. Typed
  `buffer_bytes`, default **~16–32 MiB on ingest** (HD/UHD bursts overflow 384 KiB → continuity
  errors/macroblocking), ~256 KiB–1 MiB egress. **Caveat (verified):** the kernel **doubles** the
  request then **caps at `net.core.rmem_max`** (often 208 KiB) — so a 32 MiB request silently
  clamps; **read back the granted `SO_RCVBUF` and warn** if far below request (`SO_RCVBUFFORCE`
  needs `CAP_NET_ADMIN`).
- `fifo_size` — a **count of 188-byte packets, NOT bytes** (default `7*4096`); FFmpeg's userspace
  circular buffer drained by a reader thread (exists only with pthreads). Typed `fifo_packets`
  (a `PacketCount` newtype), large default, validated against a sane ceiling (a literal
  `50000000` from a blog ≈ 9.4 GB OOM).
- `overrun_nonfatal` (bool, default 0) — drop on FIFO overrun instead of `EIO`. Typed: **default
  true on ingest**.
- `localaddr` (interface IP for send **and** join, default NULL). Typed `interface`.
  **Load-bearing on multi-homed hosts:** without it libav joins on the default-gateway NIC → the
  media NIC gets nothing, no error. Warn loudly if >1 non-loopback NIC and no `interface` set.
- `sources=` (CSV → IGMPv3 INCLUDE / SSM join) and `block=` (EXCLUDE). **There is no `multicast=`
  option — never emit one.** Typed `sources: Vec<IpAddr>` (required for `232/8`), `block`.
- `ttl` (multicast-only, **send-side** — verified; do not lump with receive buffering), default
  16. Typed `ttl: u8`.
- `pkt_size` default **1472** (not a multiple of 188). Typed: **default 1316** (7×188) on egress
  for clean TS-over-UDP.
- `timeout` (µs, the bounded read window) — reused for the runtime no-traffic confirmation (§7).

**RTP (`libavformat/rtpproto.c`).** `rtp://` is a thin wrapper: it composes a child `udp://`
forwarding `localaddr`/`ttl`/`buffer_size`/`pkt_size`/`sources`/`block` and manages a paired
RTCP socket (`rtcp_port` default `rtp_port+1`). All UDP buffering/interface/SSM semantics apply;
`fifo_size`/`overrun_nonfatal` are udp-protocol options RTP layers over (verified-nuanced). Typed
`RtpOptions` adds `rtcp_port: Option<u16>`.

---

## 4. Media formats — TS-over-UDP 1316, TS-over-RTP PT 33, and the ST 2110 boundary

**TS-over-UDP (raw).** An MPEG-2 TS is fixed 188-byte packets (0x47 sync); UDP carriage packs an
integral number per datagram, conventionally **7×188 = 1316** (1316+28 IP/UDP = 1344 < 1500 MTU;
verified it is a **convention**, not a default — libav `pkt_size` defaults to 1472, so 1316 must
be set explicitly). The multicast **address** (not a PID) identifies the channel. **No RTP header,
no sequence numbers** → loss is silent and reorder undetectable at the transport layer; only the
inner 33-bit TS PCR/PTS exists. This is the existing `ts` libav demux over a multicast-joined UDP
socket — no RTP code involved.

**TS-over-RTP.** RTP/AVP static payload type **33 = MP2T, 90 000 Hz** (RFC 3551 Table 5 —
verified; the static registry is closed); payload format RFC 2250 §2 = "an integral number of
188-byte TS packets" (a robust depacketizer accepts **any** whole multiple of 188, not exactly
7). The win over raw UDP: the RTP 16-bit **sequence number** (loss/reorder detection) + the
90 kHz timestamp. We strip the RTP header with the pure `RtpPacket::parse` (`st2110/rtp.rs`) +
RFC-1982 `seq_distance`, then feed the same TS demux. The **proto token** (`RTP/AVP` vs `udp`),
**not the PT**, selects framing — config is explicit (`Rtp` vs `Udp`), we never sniff; libav's
`rtp://` handles depacketize+demux natively for the single-path case.

**ST 2110 scope boundary** (the line is the **payload + timing regime**, not the transport):
**in scope** — RTP/UDP multicast carrying a **compressed** MPEG-TS payload, opened via libav,
free-running on `CLOCK_MONOTONIC` like every source (inv #1). **Out of scope** — uncompressed
ST 2110-20 (RFC 4175 pgroups, 1.5–6+ Gbps, hard PTP/ST 2059 lock, raster assembly). ST 2110 is
RTP-over-UDP, multicast-dominant **but also mandates unicast** (ST 2110-10:2022 — verified),
PTP-timed. **Reuse** from `st2110`: the pure `RtpPacket::parse`; RFC-1982 seq math; the bounded
drop-oldest `ChannelPacketSource`/`channel_bridge` isolation (inv #10); the essence-agnostic
`HitlessReconstructor<P>` + `DualPathPacketSource` for ST 2022-7 redundancy of redundant
**compressed RTP** feeds (the reconstructor is generic over payload P and keys purely on RTP seq —
verified it operates at the datagram layer, so it merges TS-over-RTP exactly as it merges
-20/-30/-40). **Do not reuse** the -20 raster path or the PTP lock. **Note:** ST 2022-7 needs RTP
sequence numbers — it works for `rtp://` (PT 33) but **not** bare `udp://` TS (no per-packet seq);
dual-path raw-UDP-TS would need TS continuity-counter heuristics (out of scope; documented).

---

## 5. Ingest architecture

Typed `SourceKind::Udp { url?, udp: Option<UdpOptions> }` and
`SourceKind::Rtp { url?, rtp: Option<RtpOptions> }` (internally tagged, `#[non_exhaustive]`,
mirroring `RtspOptions` on `Rtsp`). Structured fields (newtypes, never raw strings on the wire):
`group: MulticastGroup` (Ipv4/Ipv6, validated ∈ `224.0.0.0/4` or `ff00::/8`, **not**
`224.0.0.0/24`), `port`, `sources`/`block: Vec<IpAddr>`, `interface: Option<MulticastIface>`,
`buffer_bytes`, `fifo_packets`, `overrun_nonfatal`, `pkt_size`; `Rtp` adds `rtcp_port`.
**Back-compat:** a bare `url` is still accepted (so `Ts { url: "udp://@…" }` keeps working and
migrates), but the typed fields are the validated source of truth.

**Flow.** A new `ingest_plan_for` arm routes `Udp`/`Rtp` to a new `SourceLocation::Multicast`
(or reuses `SourceLocation::Url` after derivation); `open_and_stream` **derives** the libav URL
from typed config (`udp://@group:port?localaddr=…&sources=…&buffer_size=…&fifo_size=…&overrun_nonfatal=1`
or `rtp://…`) and passes it to `input_with_dictionary` exactly as `Ts` does, plus the existing
`rw_timeout` + `AVIOInterruptCB` (ADR-0030) so a stalled socket **fails open** rather than hanging.
SSM source-filtering is delegated to libav `sources=` for the single-path case; the `st2110`
`RtpReceiver::join_multicast_v4` is the seam if a manual socket path is ever needed (a socket2
`IP_ADD_SOURCE_MEMBERSHIP` extension is a future, feature-gated upgrade — noted, not built).

**Invariants.** A multicast receive socket is an **input jitter buffer, never a pacer** — the
kernel `SO_RCVBUF` + libav FIFO absorb burst/reorder; the output clock **samples** the latest
reassembled frame each tick and re-stamps PTS from the tick (`out_pts = f(tick)`, inv #1/#6). The
TS 33-bit PTS is unwrapped by the existing `PtsNormalizer` (`WrapBits::Mpeg33`, inv #3); the RTP
90 kHz clock only **informs** per-input normalization, never slaves the master clock (the same
"PTP informs, never paces" policy as `st2110`). A freshly-joined multicast input starts with an
**extended initial grace/RECONNECTING window** (the existing `PRIME_WAIT_BUDGET`, widened for
multicast) to absorb the normal first-packet loss while an IGMP-snooping switch programs the group
(RFC 2236 itself repeats the initial Report — verified) — do **not** flag NO_SIGNAL on the missing
first packets. Loss/no-traffic rides LIVE→STALE→RECONNECTING→NO_SIGNAL (inv #2) over the lock-free
`TileStore` + `reconnect_backoff`.

---

## 6. Egress architecture

Typed `Output::Udp { id?, url?, codec, group/port/interface/ttl/pkt_size/buffer_bytes…, gpu_pin?,
audio? }` and `Output::Rtp { … rtcp_port? … }` (mirroring the existing push outputs' shape,
`#[non_exhaustive]`). `build_outputs` (`pipeline.rs:3203`) gains arms: `Output::Udp` →
`RunnableOutput::Push { sink: PacketMuxSink::push(PushProtocol::UdpTs, url), label: "udp", url }`
— wiring the **already-implemented-but-unreachable** `PushProtocol::UdpTs` (`sink.rs:1028`, muxer
`mpegts`); `Output::Rtp` → a **new** `PushProtocol::Rtp` (muxer `rtp_mpegts` for TS-over-RTP PT 33
fan-out). The URL is **derived** from typed config (`pkt_size=1316`, `ttl` per scope, `localaddr`
for multi-homed).

**Invariants.** Encode-once-mux-many (inv #7) — the canvas is encoded once and the **same**
packets fan to the multicast sink alongside RTSP/HLS/NDI/SRT, re-stamped from the tick (inv #1/#6),
never from input PTS. The send is bounded/non-blocking: a slow/absent receiver on the segment must
never stall the fan-out — the existing `SINK_WEDGE_GRACE` (2 s, `pipeline.rs:138`) bounds the
per-packet `send_timeout` and the per-sink join; a wedged egress muxer is detached, never
back-pressures the output clock (inv #10). A multicast egress is inherently readable by any joiner
on the segment (no transport auth) — flag confidential-content egress (prefer admin-scoped `239/8`
on an isolated VLAN, or unicast/encrypted, when confidentiality matters).

---

## 7. Container / host-networking health warning (the operator's explicit ask)

**The problem (verified — the headline requirement).** On Docker's **default bridge** the IGMP
membership Report never leaves the container's net namespace (moby/moby#23659), so a joined group
receives **zero** external-LAN traffic and sent multicast never reaches the LAN — yet the libav
`udp://`/`rtp://` open and the IGMP join **succeed at the socket layer with no error**. The symptom
is a **silent dead input** that looks like a broken source, not a networking limitation. VS Code
Dev Containers / GitHub Codespaces default to bridge (`runArgs` defaults to `[]`); a **cloud
Codespace** runs on infrastructure with no on-prem LAN multicast at all; and `--network=host` in a
devcontainer is known to break VS Code extension install (vscode-remote-release#9212) — so we
**cannot assume** the operator can simply turn it on. Working modes: `--network=host`, or
macvlan/**ipvlan-L2** (verified-nuanced: ipvlan/macvlan **L3** mode drops all multicast — only the
L2 variants forward it; a macvlan container can't reach its own host). The engine must degrade
gracefully and **warn**.

**The design (modelled on [ADR-0035](../decisions/ADR-0035.md) sense→detect→warn).** Layered:

- **Layer A — cheap startup heuristic** (advisory `WarningCode::ContainerNoHostNetworking`). Fires
  **only on the conjunction**: `config_uses_multicast(config)` **and** `in_container()` **and**
  `!has_host_networking()`. Advisory because it cannot *prove* the LAN is silent — it flags a
  high-risk environment so the operator isn't surprised. Latched (a build-time fact, cannot flap).
- **Layer B — authoritative runtime confirmation** (`WarningCode::MulticastJoinedNoTraffic`). When
  a multicast source's join succeeded but the input's last-good-frame store received **zero bytes
  within a bounded window** (libav `timeout` + the `TileStore` freshness clock), raise a specific
  diagnostic; it **clears** when traffic resumes. This is the **authoritative** signal — and it
  also fires on a **real host** when the cause is a wrong/missing `interface`, a missing SSM source
  on a `232.x` group, or no IGMP querier, so its remediation names all of those, not just
  containerization. Pure instrumentation on the store's last-write timestamp; never blocks the
  receive loop or the clock — the compositor keeps emitting last-good/placeholder frames while it
  fires (inv #1/#2/#10).

**Detection signals (layer many — no single one is reliable; verified-nuanced).**
`in_container()` (any hit): `/.dockerenv` (Docker; **absent at buildx build time**),
`/run/.containerenv` (Podman), a **substring scan of both** `/proc/1/cgroup` and
`/proc/self/cgroup` for `{docker, containerd, kubepods, lxc}` (handles cgroup v1 `/docker/<id>`
**and** survives cgroup v2's unified `0::/`, which carries no token — so this is one signal among
several), the `container=` var read from **`/proc/1/environ`** (it lives on PID 1, not inherited;
systemd/Podman set it, plain Docker does not), `KUBERNETES_SERVICE_HOST` (kubelet),
`CODESPACES`/`REMOTE_CONTAINERS`/`DEVCONTAINER`. macOS: a native process is not "in" Docker
Desktop's hidden Linux VM ⇒ effectively on-host; use `getifaddrs` + the Layer-B signal only.
`has_host_networking()`: **(1)** namespace identity — `readlink(/proc/self/ns/net)` vs
`readlink(/proc/1/ns/net)` match under `--network=host`; **(2)** interface/route heuristic —
host mode shows the host's real NICs + LAN IP, bridge shows a single veth `eth0`. **Correction
(verified):** Docker's default bridge is **`172.17.0.0/16` (`docker0`)**, **not** `172.16.0.0/12`
(the /12 is the RFC-1918 *pool* Docker carves bridges from). The single strongest
application-level signal: the operator-configured `interface` is simply **absent** from the
container's interface list.

**Exact codes + remediation.**
`ContainerNoHostNetworking` (kebab `container-no-host-networking`, severity Warning, subsystem
`network`): *"Running in a container without host networking — Docker's default bridge does not
pass IGMP/multicast to/from the LAN, so the group is joined but no packets arrive (a silent dead
input). Restart with `--network=host` or use a macvlan/ipvlan-L2 network; in a devcontainer set
`runArgs:["--network=host"]`. Note GitHub Codespaces and cloud VMs (AWS/Azure/GCP) carry no LAN
multicast at all."* `MulticastJoinedNoTraffic` (kebab `multicast-joined-no-traffic`, subsystem
`ingest`): *"Joined multicast group {G} but received no packets within {window}. Check host
networking (`--network=host`/macvlan-L2), IGMP/IGMPv3 reachability and that an IGMP querier exists
on the segment, the ingress `interface` on a multi-homed host, and — for a `232.x` SSM group —
that a `sources` address is configured."* Both `#[non_exhaustive]` additions to `WarningCode` +
`as_str` arms + AsyncAPI/OpenAPI registration.

**Where it lives.** Detection: a new **pure** `multiview-cli/src/network_warn.rs` (`in_container`,
`has_host_networking`, `config_uses_multicast`, returning a small `NetworkMismatchView` — the
analogue of ADR-0035's `CompositeMismatchView`); no FFI, default GPU-free build. Emit:
`emit_network_warnings(publisher, view, since)` in `multiview-control` mirroring
`emit_capability_warnings` (`warning_ingest.rs:60`); build-site `probe_and_emit_network` in the CLI
alongside `capability_warn::probe_and_emit` (`main.rs:269`). Storage/surface: the existing
`InMemoryWarningStore` + `GET /api/v1/health` + the realtime stream — **unchanged** (keyed by
`as_str`).

**Never false-positives.** The warning is **conjunctive** — container **and** non-host-networking
**and** a multicast config; any one false ⇒ **silence**. A bare-metal host, a correct
`--network=host`/macvlan-L2 container, or a deliberate bridge-only deployment with no multicast
configured all emit **nothing**. **Never auto-fail, never auto-disable** (a limited input must not
stall output, inv #1, and must not be silently removed). The startup heuristic is advisory because
it cannot prove silence; the runtime joined-but-zero-bytes signal is the authoritative judgement
and is correct on a host too (it then points at interface/SSM/querier). Latched + coalescing ⇒ no
flapping.

---

## 8. Reliability + security

**Reliability.** Receive-buffer sizing: `SO_RCVBUF ≈ bitrate/8 × burst_seconds`; 20 Mbps × 100 ms
≈ 250 KB, which **exceeds** the stock `net.core.rmem_default` (≈ 208 KiB — verified via live host
read) — so the kernel **default buffer** is undersized for HD/UHD bursts, and a large request is
**silently clamped at `net.core.rmem_max`** (the kernel doubles the request, then caps it at
`rmem_max` — these are distinct sysctls that happen to share the 212992 default). Default `buffer_bytes` ~16–32 MiB on ingest, **read back the
granted size and warn** if far below request (document raising `net.core.rmem_max`); set
`overrun_nonfatal=1` + a large `fifo_packets` so a burst degrades to dropped packets, not memory
growth or a stalled engine. Interface binding (`localaddr`) is mandatory-by-recommendation on
multi-homed hosts. First-packet-after-join loss is normal — extended initial grace (§5). No querier
→ pruned → surfaced as the runtime no-traffic diagnostic, not a software fix.

**Redundancy.** ST 2022-7 hitless dual-path (identical RTP datagrams on two paths, merged by RTP
seq, no gap on single-path loss) — the recommended source redundancy for RTP media. The existing
`HitlessReconstructor<P>` + `DualPathPacketSource` are essence-agnostic and directly reusable for
redundant **TS-over-RTP** feeds; enable only when two genuinely independent paths carry the same
`rtp://` stream. Bare `udp://` TS cannot be 2022-7-merged (no per-packet seq).

**Security.** Multicast has **no transport auth** — any host on the segment can join (eavesdrop),
send to a group (inject/spoof), or flood (DoS/amplification). Defenses: **(1)** eavesdrop → scope
to admin `239/8` on an isolated VLAN; flag confidential egress. **(2)** inject/spoof → IGMPv3/MLDv2
**SSM source filtering** (the kernel delivers only the named `(S,G)` sender's packets); require
`sources=` for any `232/8` group. **(3)** flood/DoS → a **bounded** receive path (bounded
`SO_RCVBUF` + bounded drop-oldest channel) so a flood degrades to drops, never memory growth or a
stalled engine (inv #10) — the existing `channel_bridge` enforces this; add the explicit
`SO_RCVBUF` cap. Note IGMPv2-only networks silently ignore `sources=` — warn if `sources=` is set
on a non-`232` group; document the IGMPv3/MLDv2 requirement.

> **Note on the existing `channel_bridge` drop-oldest (`transport.rs:171`).** On a full channel
> the *just-received (newest)* unit is currently dropped (the in-code comment rationalizes this as
> "the reader consumes the front"). For a freshness-sensitive jitter buffer a **true drop-oldest
> ring** (evict the oldest queued unit, enqueue the newest) is preferable — the shared fix the
> AES67 backlog already plans (AES67-4). Multicast ingest reuses that fixed seam.

---

## 9. Crate / feature / config surface

**Crates.** `multiview-config` — `SourceKind::Udp`/`Rtp` + `UdpOptions`/`RtpOptions`,
`Output::Udp`/`Rtp`, the `MulticastGroup`/`MulticastIface`/`PacketCount` newtypes + address-range
validation (pure, default build). `multiview-output` — `PushProtocol::Rtp` (+ `muxer_name`
`rtp_mpegts`); `UdpTs` already exists. `multiview-cli` — `ingest_plan_for`/`open_and_stream` arms
+ the URL-from-typed-config derivation; `build_outputs` arms reaching `PushProtocol::UdpTs`/`Rtp`;
the new pure `network_warn` module + `probe_and_emit_network` build-site call. `multiview-events`
— `WarningCode::ContainerNoHostNetworking` + `MulticastJoinedNoTraffic` (+ `as_str` arms + spec
registration). `multiview-control` — `emit_network_warnings` + `NetworkMismatchView` (mirroring
`emit_capability_warnings`/`CompositeMismatchView`); the store + `GET /api/v1/health` need **no**
change. `multiview-telemetry` — multicast socket metrics (packets received/dropped, granted
`SO_RCVBUF`, no-traffic state). `multiview-input` — (future) the manual SSM socket path if libav
`sources=` proves insufficient; the runtime no-traffic confirmation reads the `TileStore` freshness
clock.

**Features.** Multicast ingest/egress rides the **existing `ffmpeg` feature** (libav `udp`/`rtp`,
LGPL-clean per ADR-0031) — **no new feature flag, no GPL escalation**. The detector + classifier +
URL derivation are pure-Rust, in the default GPU-free CI build. A manual-socket SSM path (if built
later) is feature-gated like `st2110`.

**Config** (mirrors `RtspOptions`: an `Option<Opts>` beside `url`, default-skipped, internally
tagged, back-compat):

Examples lead with IPv6 (the primary path); the IPv4 forms are the **legacy** peer.

```toml
# IPv6-first (primary): IPv6 admin-scoped (ff08::/org-local) + SSM (ff3e::/global) groups.
[[sources]]
id = "feed-a"
kind = "udp"
group = "ff08::1:10"            # IPv6 multicast (no TTL — scope is in the address)
port = 5004
interface = "2001:db8:0:1::5"   # localaddr — the media NIC (IPv6)
buffer_bytes = "24MiB"
fifo_packets = 65536
overrun_nonfatal = true

[[sources]]
id = "feed-b"
kind = "rtp"
group = "ff3e::1:2:3"           # IPv6 SSM (FF3x::/32) → sources REQUIRED (MLDv2 INCLUDE)
port = 5006
sources = ["2001:db8:0:2::7"]
interface = "2001:db8:0:1::5"

[[outputs]]
kind = "udp"
group = "ff08::2:20"            # IPv6 admin-scoped
port = 5008
codec = "h264"
pkt_size = 1316                 # 7×188

# Legacy IPv4 (legacy interop only; IPv4 is on a deprecation path — note ttl is IPv4-only):
#   [[sources]] kind="udp" group="239.255.1.10" port=5004 interface="10.20.0.5"
#   [[sources]] kind="rtp" group="232.1.2.3"    port=5006 sources=["192.0.2.7"]   # SSM
#   [[outputs]] kind="udp" group="239.255.2.20" port=5008 ttl=1
```

---

## 10. Test strategy (no special hardware)

- **Pure/unit (default build, no NIC):** the address-range classifier (`224.0.0.0/4` accept;
  `224.0.0.0/24` reject; `232/8` require sources; `239/8` scoped; `ff00::/8` by scope nibble;
  `FF3x::/32` SSM; reject `232.0.0.0`/`FF3x::4000:0000`) — property-tested. The
  URL-from-typed-config derivation (deterministic `@` placement; `sources=`/`block=`/`localaddr=`/
  `ttl=`/`fifo_size=`/`buffer_size=`/`pkt_size=`; never `multicast=`; `fifo_packets` in 188-byte
  units; bare-`url` back-compat passthrough). Config serde round-trip (TOML+JSON, internally
  tagged). `config_uses_multicast` (true only for udp/rtp multicast). Container detection over
  fixtures (cgroup v1 `/docker/<id>`, cgroup v2 `0::/`, Podman `/run/.containerenv`, PID-1
  `container=`, clean-host negative; the ns-inode compare; the bridge-IP heuristic). **No-false-
  positive tests:** non-multicast config → 0; multicast + host-net → 0; bridge + no-multicast → 0.
- **Loopback integration (CI, no hardware):** multicast on `lo`/`dummy0` — a sender + the libav
  `udp://@…`/`rtp://…` receiver, verify ingest produces frames and egress fans the same packets
  (inv #7); verify the runtime no-traffic confirmation fires on a silent sender. Real
  LAN-multicast + ST 2022-7 dual-path stay `#[ignore]`'d / hardware-runner-gated (like the
  `st2110` test).
- **Isolation chaos gate (inv #1/#10):** a flooding/wedged multicast receive socket **and** a
  stalled multicast egress sink leave the output clock ticking and no channel back-pressures the
  engine, reusing the existing chaos-gate harness + `SINK_WEDGE_GRACE`. The `SO_RCVBUF` readback
  test: request a large buffer, assert the granted-vs-requested warning fires when clamped.

---

## 11. Open questions

- **IPv6 multicast is the primary path (not a follow-up).** libav `sources=`/`block=` map to the
  family-agnostic `MCAST_JOIN_SOURCE_GROUP` for IPv6 too (bracketed `udp://@[ff3e::…]`/`rtp://[…]`),
  so the libav path is IPv6-capable now; the as-built manual `st2110` socket path is
  `join_multicast_v4` **only**, so an owned `join_multicast_v6` / `MCAST_JOIN_SOURCE_GROUP` is
  **required** (IPV6-first per [ADR-0042](../decisions/ADR-0042.md); tracked as IPV6-7 in
  [ipv6-first-backlog](../development/ipv6-first-backlog.md)). Open sub-question: full manual IPv6
  SSM fidelity vs delegating SSM joins to libav — *default:* derive bracketed IPv6 URLs and let
  libav do the MLDv2 INCLUDE join for the single-path case; the owned `socket2` IPv6 SSM path is the
  upgrade for the manual socket.
- **`SO_RCVBUFFORCE` / `CAP_NET_ADMIN`:** raising `SO_RCVBUF` beyond `rmem_max` needs
  `CAP_NET_ADMIN`. *Default:* request + readback-warn + document raising `rmem_max` (no privileged
  syscall by default); `SO_RCVBUFFORCE` behind an opt-in is a follow-up.
- **RTP push muxer name:** TS-over-RTP egress uses libav `rtp_mpegts` — **verify it is enabled in
  the linked LGPL build at MC-4 implementation time** (the protocol set is `udp,rtp,rtsp`; confirm
  the muxer).
- **Runtime no-traffic window default:** what window balances fast feedback against a slow-to-
  program IGMP-snooping switch? *Default:* align with the widened `PRIME_WAIT_BUDGET` + libav
  `timeout`; tune on the hardware runner.
- **Byte-level freshness for Layer B:** `MulticastJoinedNoTraffic` needs a *received-bytes* (not
  *decoded-frame*) freshness signal at the ingest seam, or a high-GOP feed mid-prime looks
  identical to "zero bytes" — may require a small addition to the ingest loop, not just
  instrumentation (flagged for MC-7).

---

## 12. Citations

- **Multicast / IGMP / SSM:** RFC 5771 (IANA IPv4 multicast, BCP 51), RFC 1112 (IGMPv1 + link-local
  TTL-independence), RFC 2236 (IGMPv2 + initial-Report repetition), RFC 3376 (IGMPv3 source
  filtering), RFC 4607 (SSM, `232/8`, `FF3x::/32`), RFC 4604 (IGMPv3/MLDv2 for SSM), RFC 2710/3810
  (MLDv1/v2), RFC 2365 (admin `239/8`), RFC 4291 (IPv6 multicast format/scope), RFC 1918 (the
  `172.16/12` correction).
- **RTP / TS:** RFC 3551 (RTP A/V profile, PT 33 = MP2T/90000, STD 65), RFC 2250 (MPEG over RTP,
  integral 188-byte packets).
- **FFmpeg:** ffmpeg-protocols (udp/rtp option semantics), `libavformat/udp.c` + `rtpproto.c`
  (AVOption names/defaults, `SO_RCVBUF` doubling/clamp, `fifo_size` 188-byte units).
- **SMPTE:** ST 2110-10/-20:2022 (RTP-over-UDP, multicast **+ unicast** mandate, PTP/ST 2059),
  ST 2022-7:2019 (seamless protection switching).
- **Linux/container:** `man socket.7` (`SO_RCVBUF` doubling/`rmem_max`/`SO_RCVBUFFORCE`), systemd
  `CONTAINER_INTERFACE` (`container=` PID-1 var); Docker engine/network docs (bridge default; host
  removes isolation; ipvlan/macvlan **L3 drops multicast**), moby/moby#3043 + #23659 (IGMP join
  never leaves a bridge container), containers.dev `runArgs` defaults `[]`, vscode-remote-release#9212
  (host-net breaks extension install).
- **In-repo:** [ADR-0035](../decisions/ADR-0035.md) (sense→detect→warn), [ADR-0030](../decisions/ADR-0030.md)
  (`AVIOInterruptCB`), [ADR-0031](../decisions/ADR-0031.md) (LGPL `udp`/`rtp`), the `st2110` module
  (RTP/2022-7 prior art); `schema.rs:175/213/264/531`, `sink.rs:1020/1028/1037`,
  `pipeline.rs:138/3203/3519/3762/3837/4219/4268/4275`, `event.rs:284/307`,
  `warning_ingest.rs:38/60`, `capability_warn.rs:111`, `routes/health.rs`, `st2110/transport.rs:104/149/297/410`.
