> **Design brief — IO/Transport.** Authoritative research/design record backing the
> implementation. Produced by a verification-hardened multi-agent research workflow
> (2026-06-13). Canonical crate/API naming lives in
> [docs/architecture/conventions.md](../architecture/conventions.md). The decision derived
> from this brief is [ADR-0095](../decisions/ADR-0095.md). Cross-links:
> [streaming-gotchas](streaming-gotchas.md) (the timing/robustness runbook this transport
> must obey), [srt-transport](srt-transport.md) (the **sibling** transport whose tiered seam
> this brief deliberately mirrors), [efficiency](efficiency.md) (the per-stage budget).

---

# RIST transport (input + output): a tiered seam over FFmpeg `rist://` + librist

**Audience:** engineers wiring RIST (Reliable Internet Stream Transport) into Multiview as a
first-class **ingest and egress** transport, peers of SRT/RTMP. This brief decides *how* we
integrate RIST (FFmpeg protocol layer vs direct librist FFI), what the config schema looks like,
how it aligns with the canonical invariants, and exactly what is in/out of the first
implementation slice. It is verification-hardened: every load-bearing claim about RIST profiles,
librist's license, and what FFmpeg's `librist` protocol does (and **does not**) expose was checked
against the VSF specifications and the FFmpeg source, and the adversarial findings are called out
inline (§9).

> **RIST here = the network transport (VSF TR-06).** It carries an MPEG-TS payload over RTP/UDP
> with ARQ packet recovery; it is unrelated to subtitles, to "RIS", or to any vendor product name.
> We describe it from the open VSF Technical Recommendations and the BSD-2 reference library only;
> no proprietary or trademarked vendor feature is copied (CODE_OF_CONDUCT, conventions §7).

---

## 0. TL;DR (the decision in one screen)

- **What RIST is:** an **open VSF standard** (TR-06 family) for reliable contribution/distribution
  of live media over the public internet. The media rides **RTP over UDP**; reliability comes from
  **NACK-based ARQ retransmission** (the receiver detects gaps by RTP sequence number and requests
  re-sends within an **RTT-derived buffer window**). It is the open-standard peer of SRT.
- **Profiles:** **Simple Profile** (VSF TR-06-1) = RTP + RTCP NACK ARQ + **bonding/load-sharing**
  (SMPTE 2022-7-style seamless multi-link). **Main Profile** (VSF TR-06-2) adds **GRE tunnelling**,
  **encryption** (DTLS + pre-shared-key AES), **multiplexing**, **NULL-packet removal**, and
  **in-band control**. **TR-06-3** is the **Advanced Profile** and adds **EAP-SHA256-SRP6a
  authentication** (username/password, no certificates). TR-06-4 is the GRE-tunnel-as-VPN profile
  (out of scope for media I/O).
- **License (verified):** **librist** (the reference C library, maintained at VideoLAN by SipRadius)
  is **BSD-2-Clause** — permissive, LGPL-compatible. FFmpeg's `--enable-librist` does **not** force
  `--enable-gpl` or `--enable-nonfree`; the default LGPL-clean build stays clean. RIST therefore
  belongs in the **permissive/LGPL-clean** tier alongside libsrt (MPL-2.0), **not** in the
  license-escalating tier with `gpl-codecs`/NDI.
- **Recommended implementation path — a TIERED SEAM (mirrors SRT / ADR-0039):**
  - **Tier-0 (ship first, the baseline):** route RIST through **FFmpeg's `librist` protocol** — a
    `rist://` AVIO URL opened by the existing `Demuxer` (ingest) and written by the existing
    `PacketMuxSink`/`PushProtocol` (egress, `mpegts` muxer, exactly like SRT/UDP). This is the
    cheapest path and fits the established demux/mux seam with **no new FFI**. It covers
    **single-link Simple + Main Profile with pre-shared-key AES-128/256 encryption** and the
    RTT/buffer ARQ window — which is the 90% case for contribution/distribution.
  - **Tier-1 (own what FFmpeg structurally cannot):** **link statistics** (retransmit/loss/RTT/
    bandwidth) surfaced to telemetry + the `HealthWarning` catalog. **Verified:** FFmpeg's `librist`
    protocol exposes **no** `rist_stats` callback and **no** stats AVOption — so a stats surface
    requires **direct librist FFI** (`rist_stats_callback_set`). Build this on a thin owned
    `multiview-rist-sys` leaf, the ADR-0028 own-the-FFI pattern.
  - **Tier-2 (defer; only-if-required):** **bonding / seamless path switching / load-sharing**
    across multiple ISPs. **Verified:** FFmpeg's `librist` protocol calls `rist_peer_create()`
    **exactly once** against **one** parsed URI — it has **no** multi-peer / bonding support. librist
    itself *does* support SMPTE-2022-7-style bonding (multiple `rist_peer_create()` calls into one
    `rist_ctx`). So bonding is **only** reachable via direct librist FFI, not via the FFmpeg protocol.
    Defer it behind the same `multiview-rist-sys` leaf, built only when a real multi-link
    requirement lands — exactly how SRT defers its libsrt-bonding leaf (ADR-0039 Tier-2 / ADR-0028).
- **Schema:** a typed `SourceKind::Rist { url, rist: Option<RistOptions> }` (ingest) and
  `Output::Rist { id, url, codec, gpu_pin, audio }` (egress), with a **typed `RistOptions`**
  (`profile`, `buffer_ms`, `pkt_size`, `encryption` cipher + `secret_ref`, `bonding` peer list for
  Tier-2) — never a raw opaque URL with query soup. The typed config is *lowered* to the
  `rist://?...` AVIO URL Tier-0 understands, and (Tier-2) to a librist peer list. Secrets are
  **reference-only** (`secret_ref`, the existing `SourceAuth`/secret-manager pattern); the PSK
  passphrase never lives in the config file or in logs.
- **Feature flag:** a new **`rist`** feature (off by default), `kebab-case`, gating the librist
  native dependency. Tier-0 only needs the system FFmpeg to be built `--enable-librist` (our
  own-built FFmpeg, ADR-0031, adds it to the LGPL-clean profile). Tier-1/2 add the
  `multiview-rist-sys` FFI leaf behind the same flag.
- **Invariants:** RIST ingest is a **sampled** input behind the per-tile last-good-frame store —
  its ARQ buffer is an **input jitter buffer, never the output clock** (inv #1/#2/#3). RIST egress is
  a **mux-many fan-out sink** fed the *same* encoded packets as every other transport (inv #7). All
  RIST→engine paths are bounded **drop-oldest** (inv #10). RIST's whole purpose — recovering lossy
  links — is the textbook case for *bad-inputs-are-the-purpose*: the ingest path rides librist's ARQ
  recovery **and** our tile state machine, and never falters when the link does.

---

## 1. What RIST is, and why we want it

RIST was started by the **Video Services Forum (VSF)** RIST Activity Group in 2017 to create a
common, **vendor-neutral, open** protocol for reliable live-media transport over the unmanaged
internet, with **interoperability between products from different vendors** as the explicit goal.
That openness is exactly why it fits Multiview's open-standards mandate (conventions §7,
CODE_OF_CONDUCT): unlike a single-vendor protocol, RIST is a published Technical Recommendation any
implementer can build against, and the reference library is permissively licensed.

RIST and SRT solve the same problem — **bulletproof contribution over lossy IP links** — by similar
means (ARQ retransmission with a receiver buffer sized to the round-trip time). Multiview already
treats SRT as a first-class ingest+egress transport (CLAUDE.md §1, ADR-0039); RIST is the
**open-standard sibling**, and broadcasters routinely require *both* because upstream/downstream
partners standardize on one or the other. Supporting RIST removes a "we can't take your feed"
gap without compromising the LGPL-clean default build.

### The transport model

```
            SENDER                                              RECEIVER
   ┌───────────────────────┐                          ┌───────────────────────┐
   │ MPEG-TS  ──► RTP/UDP   │ ───── lossy internet ──► │ RTP reorder + recovery │
   │  (seq #, timestamp)    │  ◄──── RTCP NACK ─────    │  buffer (RTT-sized)    │
   └───────────────────────┘   (Bitmask / Range NACK)  └───────────────────────┘
```

- **Media transport:** RTP over UDP (RFC 3550). RTP gives the **sequence number** that makes gap
  detection and reordering possible, and a timestamp for jitter handling.
- **Recovery:** the receiver tracks RTP sequence numbers; when it sees a gap it has not filled within
  the buffer window, it sends a **NACK** (negative acknowledgement) over **RTCP** asking the sender
  to retransmit the missing packet(s). Two NACK forms: a **Bitmask NACK** (RFC 4585) and a **Range
  NACK** (an APP-type RTCP packet). The sender keeps a retransmission history buffer and re-sends on
  request. This is classic **ARQ** — strictly better than blind FEC for the variable-loss public
  internet, because you only pay the bandwidth to re-send what was actually lost.
- **The buffer window IS the latency/recovery tradeoff:** the receiver must hold a buffer at least a
  few × RTT deep so a retransmitted packet can arrive before its play-out deadline. A bigger buffer
  recovers more loss at the cost of more end-to-end latency. TR-06-1:2020 added an optional **"RTT
  Echo"** RTCP message precisely to let the receiver measure RTT and size its buffer automatically.

> **The buffer is an input jitter buffer, not the output clock.** This is the single most important
> mapping to our architecture (inv #1). RIST's receive buffer absorbs network jitter and holds room
> for retransmits *on the ingest side*; it is conceptually identical to SRT's TSBPD buffer. The
> Multiview output clock (streaming-gotchas §0) is **never** paced by it — we *sample* the latest
> recovered frame at each output tick and hold-last-good if the buffer underruns (§5).

---

## 2. The profiles (verified split — TR-06-1 / TR-06-2 / TR-06-3)

| | **Simple (TR-06-1)** | **Main (TR-06-2)** | **Advanced / Auth (TR-06-3)** |
|---|---|---|---|
| Media transport | RTP/UDP | GRE-tunnelled RTP/UDP | as Main |
| Recovery (ARQ) | RTCP NACK (Bitmask + Range) | as Simple, inside the tunnel | as Main |
| **Bonding / load-sharing** | **YES** (SMPTE-2022-7-style; replicate=seamless, split=load-share) | as Simple | as Main |
| Encryption | — | **DTLS** + **pre-shared-key AES** | as Main |
| Multiplexing | one flow | **multiple flows in one GRE tunnel** | as Main |
| NULL-packet removal | — | **YES** | as Main |
| In-band control / keep-alive | — | **YES** (GRE-framed) | as Main |
| Authentication | — | (PSK / DTLS cert) | **EAP-SHA256-SRP6a** (username/password, de-authorize on the fly, no SSL certs) |
| RTT Echo (auto-buffer) | optional (2020 rev) | as Simple | as Main |

**Load-bearing nuance (verified, easy to get wrong):** **bonding and load-sharing are a Simple-Profile
feature**, not a Main-Profile-only feature. The 2020 RIST Forum material ("RIST Bonding and
Load-Sharing Demystified") states bonding/load-sharing were incorporated into the **Simple Profile**
(TR-06-1, Sept 2018). It is built on the **common RTP source**: the same RTP stream (preserving the
RTP header + payload, with possibly-different IP/UDP headers) is delivered over more than one
path, and the receiver **re-aggregates by RTP sequence number into a unified buffer**.
- **Bonding (seamless):** *replicate* every packet on every link → the receiver picks the first copy
  of each sequence number. Survives a full link failure with zero loss (SMPTE-2022-7 principle, plus
  RIST's ARQ on top). Costs N× bandwidth.
- **Load-sharing:** *split* packets across links → one copy each, distributed. Lower bandwidth, less
  per-link pressure; recovery leans on ARQ when a link degrades.
- **Use case:** combining heterogeneous internet (cable + ADSL + multiple **cellular modems** + Wi-Fi)
  for remote contribution where no single link is reliable. This is the headline RIST selling point.

**Why this matters for our tiering:** bonding lives in the *protocol*, but **librist exposes it only
through multiple `rist_peer_create()` calls**, and FFmpeg's protocol layer makes exactly one (§9).
So even though bonding is "Simple Profile," we cannot reach it via the FFmpeg path — it is the
Tier-2 direct-FFI feature.

**Encryption / auth scope decision:** Tier-0 ships **pre-shared-key AES (128/256)** — it is the one
encryption mode FFmpeg's `librist` protocol exposes (`secret` + `encryption`, §3/§9), it requires no
certificate plumbing, and it covers the common "both ends share a passphrase" deployment. **DTLS**
and **EAP-SRP** (TR-06-3) need certificate/credential machinery librist supports but FFmpeg does not
surface; they are **deferred to the Tier-1/2 direct-FFI leaf** and only built when a deployment
needs them. We document this honestly rather than pretending Tier-0 does DTLS.

---

## 3. How RIST maps onto the existing transport seam

The repo already has the exact shape RIST needs — RIST is a near-clone of how SRT is wired, which is
the strongest possible argument for the Tier-0 FFmpeg path. The as-built facts (verified by reading
the source):

### Ingest (a `rist` SourceKind)

- `SourceKind` is an internally-tagged serde enum (`#[serde(tag = "kind", rename_all = "snake_case")]`,
  `#[non_exhaustive]`) in `crates/multiview-config/src/schema.rs`. Network sources (`Rtsp`, `Hls`,
  `Ts`, `Srt`, `Rtmp`) all carry a `url: String` and flow through **one** libav demuxer.
- The demuxer (`crates/multiview-ffmpeg/src/demux.rs`, `Demuxer::open`) calls
  `ffmpeg::format::input(&path)`; libav auto-routes by URL scheme. A `srt://…` URL needs **zero**
  custom code — and a `rist://…` URL is identical: libav routes it to the `librist` protocol when
  the FFmpeg build has `--enable-librist`. The `FileSource` adapter
  (`crates/multiview-input/src/libav.rs`) already documents "RTSP/HLS/TS/SRT/RTMP all flow through
  the same demuxer"; RIST joins that list.

**Add:**
```rust
/// RIST (Reliable Internet Stream Transport, VSF TR-06) input.
Rist {
    /// Source URL (`rist://[::]:port` or peer host).
    url: String,
    /// Optional typed RIST options (profile, buffer, encryption, bonding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rist: Option<RistOptions>,
},
```

### Egress (a `rist` OutputKind / push sink)

- `Output` is the matching internally-tagged enum in the same file; push sinks (`Rtmp`, `Srt`) carry
  `id`, `url`, `codec`, `gpu_pin`, `audio`.
- The push machinery is `PushProtocol` (`crates/multiview-output/src/sink.rs`): a selector enum whose
  `muxer_name()` maps to a libav muxer. `Srt` and `UdpTs` both use `"mpegts"`. The CLI
  (`crates/multiview-cli/src/pipeline.rs`) matches `Output::Srt { url, .. }` → `PacketMuxSink::push(
  PushProtocol::Srt, url)` and fans the **same encoded packets** to it via `run_push_output`
  (encode-once-mux-many, inv #7).

**Add** a `PushProtocol::Rist` whose `muxer_name()` is `"mpegts"` (RIST carries an MPEG-TS payload,
exactly like SRT/UDP), an `Output::Rist { id, url, codec, gpu_pin, audio }` variant, and the CLI
match arm. **No new fan-out path** — RIST is just another consumer of the already-encoded packet
stream.

### Why the FFmpeg path "just works" for Tier-0

The transport URL is opaque to the demuxer/muxer; the scheme selects the protocol and the
`mpegts`/auto container is independent of the transport. The typed `RistOptions` lower to the
`rist://host:port?rist_profile=main&buffer_size=<ms>&encryption=128&secret=<psk>&pkt_size=1316`
AVIO URL (the option names are taken from FFmpeg's `librist` protocol, §9). Ingest and egress use the
same protocol; only the URL role differs (listen vs call), just like SRT.

---

## 4. Config schema (typed, not query-soup)

Mirror `RtspOptions`/`SrtConfig`: a typed options struct that *validates* and *lowers* to the AVIO
URL, so the operator (and the WebUI) configure RIST with named fields, not by hand-writing a fragile
`rist://?...` query string. All fields are optional with serde `skip_serializing_if`.

```rust
/// RIST-specific connection options (input or output). Lowered to a `rist://…` AVIO
/// URL for the Tier-0 FFmpeg path, and to a librist peer list for Tier-2 bonding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RistOptions {
    /// RIST profile. Default = `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<RistProfile>,          // Simple | Main | Advanced
    /// Receiver recovery/jitter buffer depth, milliseconds (the ARQ window).
    /// 0 = librist auto (RTT-Echo derived). Maps to FFmpeg `buffer_size`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_ms: Option<u32>,                // 0..=30000
    /// MPEG-TS-aligned packet size. Default 1316 (7×188). Maps to FFmpeg `pkt_size`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkt_size: Option<u16>,
    /// Pre-shared-key encryption (Main Profile). Cipher + a secret REFERENCE only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<RistEncryption>,
    /// Tier-2 only: bonding/load-sharing peer endpoints (multi-ISP). Empty = single link.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bonding: Vec<RistPeer>,
}

/// PSK encryption: the AES key length plus a secret-manager reference (never a plaintext key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RistEncryption {
    /// AES key length in bits. 128 or 256. Maps to FFmpeg `encryption`.
    pub aes_bits: RistAesBits,                 // Aes128 | Aes256
    /// Reference to the pre-shared passphrase (e.g. `op://Servers/feed/rist-psk`).
    /// Resolved at run time; NEVER stored or logged in plaintext. Maps to FFmpeg `secret`.
    pub secret_ref: String,
}
```

- **Secrets are reference-only.** The PSK passphrase is held as a `secret_ref` (the existing
  `SourceAuth { secret_ref }` pattern, resolved through the 1Password/secret-manager seam) and is
  **redacted in every URL we log** — exactly the `to_url` / `to_url_redacted` split the SRT model
  already uses. The plaintext secret only ever lives in the in-memory AVIO URL passed to libav.
- **`profile` defaults to `main`** to match librist/FFmpeg's own default, but the schema accepts
  `simple` (for bonding-without-encryption legacy peers) and `advanced` (TR-06-3, Tier-1/2 only).
- **`bonding` is parsed and validated in Tier-0 but rejected-with-a-clear-error if non-empty while
  only the FFmpeg path is compiled** — never silently ignored. Honest capability reporting, per the
  multicast brief's HealthWarning doctrine: if the operator asks for bonding on a Tier-0-only build,
  they get an explicit "bonding requires the `rist` direct-FFI build" error at validate time, not a
  single-link feed that silently drops a peer.
- **IPv6-first** (conventions §10): RIST URLs default to dual-stack listen `rist://[::]:port`,
  loopback `[::1]`, and **bracket** IPv6 literals in the lowered URL. librist binds UDP sockets;
  the listen side uses `[::]` (dual-stack) per the IPv6-first directive. Examples lead IPv6.

---

## 5. Invariant alignment (the part that must be airtight)

| Inv | RIST obligation | How |
|---|---|---|
| **#1 output-clock** | RIST ingest must NOT pace the output; RIST egress must NOT stall the clock. | Ingest = a sampled source behind the per-tile last-good store; the ARQ buffer is an input jitter buffer (§1). Egress = a push sink consuming already-encoded packets off a **bounded drop-oldest** queue (`run_push_output`); the encoder/clock never `.await`s the RIST socket. A wedged RIST egress link drops packets, it does not back-pressure the engine. |
| **#2 last-good + state machine** | A degrading RIST link must ride LIVE→STALE→RECONNECTING→NO_SIGNAL, never crash. | The decoded frames land in the framestore exactly like any other source; if the ARQ buffer underruns (loss exceeds the recovery window), the tile goes STALE/holds last-good. Supervised reconnect (ADR-R003) re-opens the `rist://` URL. |
| **#3 unified timing** | Never feed RIST input PTS to the muxer; re-stamp from the tick; never float fps. | RIST input PTS go through the standard normalize/rebase pipeline (streaming-gotchas §0); output PTS are re-stamped from the tick counter. RIST adds no new timing path. |
| **#7 encode-once-mux-many** | RIST egress is one more fan-out target, not a separate encode. | `PushProtocol::Rist` consumes the *same* encoded packets as RTMP/SRT/UDP via `run_push_output`. A separate RIST encode happens only if codec/res/bitrate differ — same rule as every other output. |
| **#10 isolation** | RIST stats/telemetry must not back-pressure the engine. | The Tier-1 stats callback runs on librist's own thread and publishes to a watch/broadcast channel (drop-oldest); the engine never reads it synchronously. The stats surface is best-effort, conflated, off the data plane. |

**bad-inputs-are-the-purpose:** RIST exists to recover lossy links, so a RIST source *will* see loss,
retransmits, and reconnects in normal operation. The ingest path must treat ARQ recovery and link
flaps as **expected behaviour**, not errors to log-spam — manage the librist log level (the FFmpeg
`log_level` option), surface link health through the Tier-1 stats→HealthWarning surface, ride the
tile state machine, and **never falter** when a link degrades. A RIST feed that loses a link and
recovers must look, from the output, exactly like a healthy tile (held last-good for the few ms of a
gap, then live again). Discarding a flaky RIST source is the **wrong** fix — recovering it is the
whole point of the protocol and of this product.

---

## 6. The implementation-path decision (FFmpeg protocol vs direct librist FFI)

This is the core engineering call. We assess both honestly, then recommend the tiered seam.

### Option A — FFmpeg's `librist` protocol (the Tier-0 baseline)

**What it is:** `libavformat`'s `librist.c` protocol handler. A `rist://` URL is opened by libav;
options are passed as AVOptions/URL query (`rist_profile`, `buffer_size`, `pkt_size`, `secret`,
`encryption`, `fifo_size`, `overrun_nonfatal`, `log_level` — §9). It calls librist under the hood.

- **Pros:** zero new FFI; reuses the *exact* demux/mux seam SRT/RTMP/UDP already use; LGPL-clean
  (librist is BSD-2, no `--enable-gpl`); minimal new code (a schema variant, a `PushProtocol` arm, a
  URL-lowering function, a CLI match); validated by FFmpeg's own conformance. Fastest to ship,
  lowest risk, smallest diff.
- **Cons (verified, §9):** **single peer only** (`rist_peer_create` called once) → **no bonding /
  load-sharing / seamless switching**. **No statistics** (no `rist_stats` callback, no stats option)
  → no retransmit/loss/RTT visibility for the HealthWarning surface. **PSK-AES encryption only** (no
  DTLS / EAP-SRP exposed). It is a thin pass-through: great for single-link, blind beyond that.

### Option B — direct librist FFI (a `multiview-rist-sys` leaf)

**What it is:** an owned `-sys` crate binding librist's C API (`rist_receiver_create`/
`rist_sender_create`, `rist_peer_create` per peer, `rist_stats_callback_set`, the data
read/write callbacks), the ADR-0028 own-the-FFI-binding pattern (the same pattern NDI and the
deferred SRT-bonding leaf use).

- **Pros:** full control — **bonding/load-sharing** (multiple `rist_peer_create` into one ctx),
  **link statistics** (the stats callback), **profile selection** including DTLS/EAP-SRP, precise
  buffer/RTT control, no FFmpeg-version dependency for RIST features.
- **Cons:** real FFI surface to own (`unsafe`, `// SAFETY:`, RAII lifecycles, callback threads that
  must not unwind across the boundary — safety rules §4); librist must be packaged/built; we now
  carry the data-read loop ourselves rather than leaning on libav's demuxer. Much larger diff and a
  new maintenance burden. Overkill for single-link feeds.

### Recommendation — the TIERED SEAM (Option A now, Option B incrementally, mirroring SRT/ADR-0039)

Ship **Tier-0 = Option A** (FFmpeg `rist://`) for single-link Simple+Main+PSK — it is the LGPL-clean,
minimal-diff, fits-the-existing-seam path and covers the dominant use case. Reserve **Option B** for
the two things FFmpeg structurally cannot do, behind the **same `rist` feature**, built **only when
required**:
- **Tier-1 = stats** (`multiview-rist-sys` + `rist_stats_callback_set` → telemetry/HealthWarning).
  This is the first Option-B increment because link health visibility is broadly useful and is the
  one capability operators will miss soonest on a lossy link.
- **Tier-2 = bonding / seamless switching** (multiple `rist_peer_create`), **deferred** until a real
  multi-ISP requirement lands — exactly as SRT defers its libsrt-bonding leaf (ADR-0039 Tier-2,
  ADR-0028). We do not build speculative bonding; we document the seam so it is a clean addition.

This is precisely the SRT precedent: "keep the license-clean FFmpeg caller baseline + own what
FFmpeg structurally can't (stats, multiplexing) + defer the FFI bonding leaf only-if-required." Using
the same tiering keeps the two transports symmetric and reviewable, and avoids a speculative FFI
build we may never need.

---

## 7. Efficiency pass (mem / cpu / io budget — the standing review)

- **Memory:** the dominant RIST buffer is the **receiver ARQ/jitter buffer** (`buffer_ms` deep). At a
  contribution bitrate of ~10–20 Mbit/s and a 1000 ms buffer, that is ~1.25–2.5 MB of *encoded* bytes
  per RIST source — trivial, and it lives **inside librist**, not in our framestore. Our per-tile
  last-good store is unchanged (one decoded NV12 frame per tile, inv #5). The Tier-1 stats struct is a
  few hundred bytes published over a watch channel (conflated). **Budget: negligible incremental
  memory; the ARQ buffer is bounded by `buffer_ms` and is the operator's latency/recovery knob.**
- **CPU:** Tier-0 adds **no compositing/encode cost** — RIST egress is encode-once-mux-many, so the
  marginal cost over "no RIST output" is one extra `mpegts` mux + UDP send of already-encoded packets
  (cheap, the SRT/UDP cost profile). Ingest adds one demux/decode like any source (budgeted in
  megapixels/s, inv #6 — decode at display resolution). librist's ARQ runs on its own thread;
  retransmit handling is RTP-sequence bookkeeping, not media processing. **Budget: ingest = standard
  per-source decode; egress = one extra mux+send per RIST output, no extra encode.**
- **IO / network:** RIST ARQ **retransmissions add upstream/return bandwidth** proportional to loss
  (re-sent packets + NACK RTCP). On a clean link this is ~0; on a lossy link it is bounded by the loss
  rate. **Bonding (Tier-2) multiplies egress bandwidth by the number of replicated links** — a real
  cost the operator must budget; load-sharing splits instead of multiplies. Document the bandwidth
  implication in the WebUI so an operator does not accidentally 3× their uplink. **Budget: clean-link
  overhead ≈ 0; lossy-link overhead = loss-rate-bounded retransmits; bonding = N× (operator's choice).**
- **No per-frame allocation on the data plane** (safety rule §5): RIST egress packets come from the
  shared encoded-packet stream; RIST ingest frames come from the standard decoder pool. The Tier-2 FFI
  read/write buffers, when built, come from a pre-allocated pool, never per-packet.

---

## 8. Testing, chaos & soak (lossy-link simulation)

RIST's correctness *is* its loss-recovery behaviour, so the tests must inject loss.

- **Conformance oracle (Tier-0):** a CI test that round-trips a known TS through `rist://` loopback
  (sender → `rist://[::1]:port` → receiver) on a `--enable-librist` FFmpeg and asserts byte/stream
  fidelity on a clean link. Mirrors the SRT conformance oracle (ADR-0039).
- **Lossy-link chaos gate:** drive the loopback through a **netem/tc impairment** (packet loss 1–10%,
  jitter, reorder, a brief full-link blackout) and assert that (a) the **output never stalls** (inv
  #1 — the tick keeps emitting), (b) the tile rides STALE→LIVE across the blackout and **recovers**
  rather than dying (inv #2, bad-inputs-are-the-purpose), and (c) the engine is **never
  back-pressured** by the RIST socket (inv #10 — the existing chaos harness). This is the most
  important RIST test: it proves the *whole point* of the protocol works through our pipeline.
- **Stats assertion (Tier-1):** with injected loss, assert the stats surface reports non-zero
  retransmits/loss and that a sustained-loss condition raises the right `HealthWarning` (and clears
  on recovery) — without ever blocking the data plane.
- **Bonding (Tier-2, when built):** two impaired loopback links; kill one mid-stream; assert
  zero-loss seamless continuation (bonding/replicate) and correct re-aggregation by RTP sequence.
- **No bit-exact GPU asserts** — RIST is a transport; fidelity is at the TS/packet level, not the
  pixel level. Soak: a multi-hour lossy-link run asserting no drift, no unbounded memory, no
  reconnect storms (the standard soak tier).

---

## 9. Adversarial verification (the load-bearing claims, checked)

Every claim below was checked against a primary source; the findings drove the tiering decision.

1. **"librist is BSD-2-Clause and LGPL-clean." — CONFIRMED.** librist is published/maintained at
   VideoLAN (by SipRadius / Sergio Ammirata) under a **liberal BSD 2-clause** license, released
   deliberately permissively "to assure its widespread adoption." FFmpeg's `--enable-librist` is an
   **LGPL-compatible external library** — it does **not** force `--enable-gpl` or `--enable-nonfree`
   (only GPL libraries do). **Verdict:** RIST belongs in the permissive/LGPL-clean tier alongside
   libsrt (MPL-2.0); the default build stays LGPL-clean; `cargo-deny` needs only BSD-2-Clause on the
   allow-list (it almost certainly already is — confirm at implementation time). This is **not** a
   license-escalating dependency like `gpl-codecs` or the proprietary NDI SDK.

2. **"FFmpeg's `librist` protocol exposes profile/buffer/encryption." — CONFIRMED.** Reading
   `libavformat/librist.c`: the AVOption array is **`rist_profile`** (0/1/2 = simple/main/advanced,
   **default main**), **`buffer_size`** (ms, default 0=auto, range 0–30000), **`fifo_size`**,
   **`overrun_nonfatal`**, **`pkt_size`** (default 1316), **`log_level`**, **`secret`** (PSK string,
   mandatory if encryption on), **`encryption`** (0/128/256). **Verdict:** the Tier-0 schema lowering
   has everything it needs for single-link Simple/Main + PSK-AES.

3. **"FFmpeg's `librist` protocol does NOT do bonding." — CONFIRMED (the decisive finding).**
   `librist.c` parses **one** URI (`rist_parse_address2`) and calls **`rist_peer_create()` exactly
   once** — there is **no peer list, no loop, no multi-path**. **Verdict:** bonding/load-sharing/
   seamless-switching is **unreachable** through the FFmpeg protocol and **requires direct librist
   FFI** (multiple `rist_peer_create` into one ctx). This is what pushes bonding to **Tier-2** and
   makes the FFmpeg-only path explicitly single-link. **If the operator's priority is multi-ISP
   bonding from day one, Tier-2 (direct FFI) is required up front — the FFmpeg path cannot deliver
   it.** (Mitigated by the schema rejecting a non-empty `bonding` list on a Tier-0-only build with a
   clear error, never silently.)

4. **"FFmpeg's `librist` protocol does NOT expose statistics." — CONFIRMED.** No `rist_stats`
   callback is registered and no stats AVOption exists in `librist.c`. **Verdict:** the link-health
   surface (retransmit/loss/RTT/bandwidth → telemetry + HealthWarning) needs the direct-FFI
   `rist_stats_callback_set` — this is **Tier-1**, the first Option-B increment.

5. **"Bonding is a Main-Profile feature." — REFUTED.** Bonding/load-sharing is a **Simple-Profile**
   capability (TR-06-1, 2018), built on the common-RTP-source / re-aggregate-by-sequence-number model
   (SMPTE-2022-7 principle). The brief reflects the corrected fact. (Encryption/GRE/multiplexing *are*
   Main-Profile-only; bonding is not.) This corrects a common misconception and is why the §2 table is
   explicit about which profile owns which feature.

6. **"DTLS is in Tier-0." — REFUTED / scoped out.** FFmpeg's `librist` protocol exposes only PSK
   (`secret`+`encryption`), not DTLS or EAP-SRP. **Verdict:** Tier-0 = PSK-AES only; DTLS/EAP-SRP are
   Tier-1/2 direct-FFI features, documented honestly, not claimed.

**Net adversarial verdict:** the FFmpeg-protocol Tier-0 path is **correct and sufficient for the
single-link contribution/distribution case** (the 90% use), is **license-clean**, and **fits the
existing SRT-shaped seam with a minimal diff** — so it is the right thing to ship first. But it is
**genuinely thin**: it cannot do bonding or stats. The recommendation is therefore explicitly a
**tiered seam**, not "FFmpeg forever" — Tier-1 (stats) and Tier-2 (bonding) require direct librist
FFI and are documented as such, with the schema honest about what each build can do. This is the same
verified conclusion the SRT brief reached, applied to RIST's specifics.

---

## 10. Scope of the first implementation slice (Definition of Done)

**In (Tier-0, ships complete and wired end-to-end):**
- `SourceKind::Rist { url, rist }` + `Output::Rist { id, url, codec, gpu_pin, audio }` in the schema,
  with the typed `RistOptions`/`RistEncryption`/`RistProfile`/`RistAesBits` and validation.
- URL lowering (`to_url` / `to_url_redacted`) from typed options → `rist://…?…` AVIO URL, secret
  resolved from `secret_ref`, redacted in logs.
- `PushProtocol::Rist` (`muxer_name` = `mpegts`) + the CLI ingest/egress wiring (the `Output::Rist`
  match arm; `Rist` source through the existing `FileSource`/demuxer path) — fanned by
  `run_push_output` (inv #7).
- The `rist` feature flag (off by default), conventions §4/§7 entries, IPv6-first defaults.
- The conformance oracle + the **lossy-link chaos gate** (the bad-inputs proof).
- Honest capability reporting: a non-empty `bonding` list on a Tier-0-only build is **rejected with a
  clear error**, never silently dropped.

**Tier-1 (own-what-FFmpeg-can't) — SHIPPED (RIST-5):** `multiview-rist-sys` FFI leaf
(runtime-loaded librist, BSD-2, never linked at build time) + `rist_stats_callback_set` on an
**owned** librist context → the `multiview-telemetry` metric series + a conflated `rist.link.stats`
realtime event + the `rist-link-loss` HealthWarning, behind a dedicated off-by-default `rist-stats`
feature. **Honest boundary:** because FFmpeg owns its `rist://` context privately, the stats-bearing
egress is a direct-librist **sender** (leaf-sized, a sink consuming already-encoded packets, inv #7);
the direct-librist **receiver** with stats (a new `Source` owning the receive+demux loop) is the
Tier-2-shaped follow-up — not built, but the decode + bridge already handle the receiver role.

**Deferred, only-if-required (Tier-2):** bonding / load-sharing / seamless switching (multiple
`rist_peer_create`) + DTLS/EAP-SRP, on the same FFI leaf. Documented as a clean seam; **not** built
speculatively (no parked stubs — the seam is the schema field + the explicit "requires direct-FFI
build" error, which is a complete, honest behaviour).

---

## 11. References

VSF Technical Recommendations and the reference library (open standards + permissive library only):

- VSF TR-06-1 (RIST Simple Profile, 2018; 2020 rev adds RTT-Echo): RTP/UDP + RTCP NACK ARQ + bonding.
  <https://static.vsf.tv/download/technical_recommendations/VSF_TR-06-1_2020_06_25.pdf>
- VSF TR-06-2 (RIST Main Profile, 2022/2024): GRE tunnel + DTLS/PSK encryption + multiplexing +
  NULL-removal + in-band control.
  <https://static.vsf.tv/download/technical_recommendations/VSF_TR-06-2_2024_06_12.pdf>
- VSF TR-06-3 (RIST Advanced Profile / authentication, 2022): EAP-SHA256-SRP6a.
  <https://static.vsf.tv/download/technical_recommendations/VSF_TR-06-3_2022-09-08.pdf>
- VSF RIST Activity Group overview. <https://potato.vsf.tv/RIST.shtml>
- RIST Forum, "RIST Bonding and Load-Sharing Demystified" (bonding is Simple-Profile;
  replicate-vs-split). <https://www.rist.tv/articles-and-deep-dives/2020/6/16/rist-bonding-and-load-sharing-demystified>
- librist (VideoLAN, BSD-2-Clause). <https://code.videolan.org/rist/librist>
- librist function reference (peer/stats/profile API).
  <https://code.videolan.org/rist/librist/-/wikis/5.-libRIST-Function-Reference>
- FFmpeg `libavformat/librist.c` (the protocol AVOptions + single `rist_peer_create`, no stats).
  <https://www.ffmpeg.org/doxygen/7.0/librist_8c_source.html>
- FFmpeg Protocols documentation (`rist` options). <https://ffmpeg.org/ffmpeg-protocols.html>
- FFmpeg License (librist is an LGPL-compatible external library, not GPL).
  <https://github.com/FFmpeg/FFmpeg/blob/master/LICENSE.md>
