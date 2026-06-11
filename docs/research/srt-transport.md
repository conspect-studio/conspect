# SRT (Secure Reliable Transport) input + output — design brief

**Status:** design (2026-06-08). Drives [ADR-0039](../decisions/ADR-0039.md); consistent
with [ADR-0031](../decisions/ADR-0031.md) (our-built LGPL-clean FFmpeg, `--enable-libsrt`),
[ADR-0034](../decisions/ADR-0034.md) (decoupled routing / `StreamInventory`),
[ADR-0035](../decisions/ADR-0035.md) (capability detection + `HealthWarning` catalog), and
the [ADR-0028](../decisions/ADR-0028.md) own-the-FFI-binding precedent (NDI).
**SRT here = the Haivision/SRT-Alliance open UDP transport (`libsrt`), NOT SubRip subtitles.**
**Question:** what is the implementation-ready architecture for **SRT contribution and
distribution — ingest *and* egress, in every connection mode** — so Multiview is a
first-class SRT endpoint (statistics, encryption, stream-id routing, listener
multiplexing, and eventually bonding), without breaking the output-clock or isolation
invariants and without escalating the default build's license?

> **Scope note.** SRT bytes already flow today through FFmpeg's `srt://` protocol, and a
> fully-built but **orphaned** typed connection model already exists in the tree. This
> brief does **not** re-argue *whether* to support SRT — it specifies *how* to turn the
> blind passthrough into a designed subsystem: the backend tiering, the modes matrix, the
> timing reconciliation, the secret handling, the stream-id routing, the
> statistics→telemetry surface, the resilience model, and the backward-compatible
> crate/feature/config surface — all grounded in the as-built code. RIST (VSF TR-06) is
> **out of scope** (§8).

---

## 0. Headline

SRT is **already a working but blind, single-mode passthrough**, and a fully-built typed
model for it **already exists, orphaned**. Bytes flow today through FFmpeg's `libsrt`
protocol: ingest `SourceKind::Srt { url }` (`schema.rs:269`) → `SourceLocation::Url`
(`pipeline.rs:3545`) → `input_with_dictionary("srt://…", { rw_timeout })` (`demux.rs:191`)
with `WrapBits::Mpeg33` PTS normalization (`pipeline.rs:4277`); egress
`Output::Srt { id, url, codec, gpu_pin, audio }` (`schema.rs:635`) → `PushProtocol::Srt`
→ muxer `mpegts` (`sink.rs:1039`) → `PacketMuxSink::push` (`sink.rs:1233`) fanned by the
tolerant `run_push_output` (`pipeline.rs:2307`, "unreachable peer dropped, never fatal" —
inv #1/#10). Meanwhile `crates/multiview-input/src/srt.rs` (332 lines, `pub mod srt` at
`lib.rs:110`, tested by `tests/srt_config.rs`) already models the **full** connection —
`SrtMode {Caller, Listener, Rendezvous}`, `KeyLength {None, Aes128/192/256}`,
`StreamId` (≤512 B), `SrtConfig { mode, host, port, key_length, passphrase, stream_id,
latency_ms }` with `validate()`/`to_url()`/`to_url_redacted()`/`percent_encode` — but it is
referenced by **nothing** on the run path (verified: no `SrtConfig` import outside its own
test).

So the load-bearing decision is **not "build SRT"** but: **(1)** connect the existing typed
`SrtConfig` to the schema in a way that stays byte-compatible with the bare `{ url }`; and
**(2)** **tier the backend** behind a socket-free seam — exactly as the preview WHEP
transport seam and the NDI-sys FFI binding already establish — to keep the license-clean
FFmpeg `srt://` path as the **default conformance baseline** while *owning* the two things
FFmpeg structurally **cannot** give us: **runtime link statistics** (`libavformat` never
calls `srt_bstats`/`srt_bistats` — verified) and **listener-mode multiplexing** (FFmpeg's
listener is `srt_listen(fd, 1)` + one `srt_accept` — verified). Throughout, SRT's TSBPD
receive buffer is an **input jitter buffer** feeding the last-good-frame store (inv #2),
**never** the output cadence (inv #1); its stats poller and listen-accept callback are
off-hot-path, drop-oldest, and physically incapable of back-pressuring the engine (inv #10).

---

## 1. As-built vs missing (verified against the tree)

**As-built (verified this session):**

| Layer | Location | What it does |
|---|---|---|
| Ingest config | `multiview-config/src/schema.rs:269` | `SourceKind::Srt { url: String }`, internal tag `srt`, flattened into `Source` (`schema.rs:293`); treated as a live, reconnecting source (`lib.rs:320`). |
| Egress config | `multiview-config/src/schema.rs:635` | `Output::Srt { id, url, codec, gpu_pin, audio }`; `codec` validated like RTMP (`lib.rs:520`). |
| Ingest byte path | `multiview-cli/src/pipeline.rs:3545` | `SourceKind::Srt { url } → SourceLocation::Url` → opened via `Demuxer`/`input_with_dictionary` with the generic `rw_timeout` (`demux.rs:191`, `INGEST_RW_TIMEOUT = 10 s`); PTS unwrapped `WrapBits::Mpeg33` (`pipeline.rs:4277`). |
| Egress byte path | `multiview-cli/src/pipeline.rs:3235` | `Output::Srt → PacketMuxSink::push(PushProtocol::Srt, url)`; `PushProtocol::Srt \| UdpTs => "mpegts"` (`sink.rs:1039`); `Muxer::create_as(Path(url), "mpegts")`; tolerant push loop (`pipeline.rs:2307`). |
| Audio carriage | `multiview-audio/src/capability.rs:31,98` | SRT → `OutputTransport::MpegTs` → `TrackSupport::Multiple` (unbounded PIDs) — N discrete audio tracks. |
| Build/licence | `docs/decisions/ADR-0031.md`, `deny.toml` | `--enable-libsrt` is in the **LGPL-clean default** FFmpeg profile; `srt` in `--enable-protocol`; `MPL-2.0` already on the `deny.toml` allow-list. |
| Tests | `multiview-output/tests/push_and_gpl.rs:163` | Only the contract `PushProtocol::Srt.muxer_name() == "mpegts"` — **no network exercise**. |
| **Orphaned typed model** | `multiview-input/src/srt.rs` (full) | `SrtMode`/`KeyLength`/`StreamId`/`SrtConfig` + `validate`/`to_url`/`to_url_redacted`/`percent_encode`; tested in `tests/srt_config.rs`; **unused by the run path**. |

**Missing (this design):**

1. **Typed schema fields on the run path** — mode, encryption, latency, stream-id are
   unreachable; only a bare `url` is configurable.
2. **Listener mode** — the as-built egress is caller-only (`PushSink` "only succeeds when a
   peer is listening", `sink.rs:1051`); there is no SRT *listener* for ingest aggregation or
   pull-egress.
3. **Any link statistics** — none are reachable through `libavformat` (the verified blocker, §7).
4. **Stream-id Access-Control routing** and listener multiplexing (§6).
5. **Bonding** (§8), **SRT-specific health/warnings** (§7), and **SRT-specific reconnect
   tuning** (only the generic 10 s `rw_timeout` exists today, §9).

**Two honest corrections to make while wiring** (verified, §4–§5):

- `SrtConfig::validate` (`srt.rs:228`) caps the passphrase at `10..=79`, but the **libsrt
  source accepts `10..=80` inclusive** (`HAICRYPT_SECRET_MAX_SZ = 80`). The `10..=79` figure
  is from Haivision's *product* docs and FFmpeg's wrapper comment, not the library. **Widen
  to `10..=80`** (and note the FFmpeg-path 79 cap when Tier-0 is the backend, §11).
- `SrtConfig::to_url` (`srt.rs:266`) emits `latency=<ms>`, but FFmpeg's `libsrt` AVOptions
  are in **microseconds** — a latent **1000×** bug. The seam must convert `×1000` at the
  FFmpeg boundary (libsrt's *native* API is milliseconds; the unit flips only in
  `libavformat`).

---

## 2. Binding decision — a tiered transport seam

A tiered `SrtTransport` seam, mirroring the **preview WHEP seam**
(`multiview-preview/src/whep/transport.rs`: `SessionId` newtype (`:61`), explicit
`SessionState` Created→Connecting→Connected→Closed (`:153`), `SampleSink`/`SampleFeed`
bounded **drop-oldest** (`:300`/`:310`)) and the **NDI-sys FFI** precedent
([ADR-0028](../decisions/ADR-0028.md)). The seam means the engine and config **never know
which tier serves a link**.

- **Tier 0 — FFmpeg `srt://` (default, `ffmpeg` feature, as-built).** The byte path for
  **caller** ingest/egress and the **conformance oracle**. **License-clean:** libsrt is
  **MPL-2.0** (weak, file-level copyleft) and sits in FFmpeg's `EXTERNAL_LIBRARY_LIST`
  (verified) — `--enable-libsrt` forces **neither** `--enable-gpl` **nor** `--enable-nonfree`,
  so the default build stays LGPL-clean. **Not a license-escalating feature**; it lives under
  the ordinary `ffmpeg` flag. Ships caller mode + URL-expressed encryption + latency +
  stream-id with **zero new native deps**, reusing the already-tested `SrtConfig::to_url`.
- **Tier 1 — pure-Rust `srt-tokio` behind the seam (off-by-default `srt` feature).** Owns the
  two FFmpeg-impossible capabilities: **(a)** `srt_bstats`-class **statistics** → telemetry/
  health (§7), and **(b)** **listener-mode multiplexing** with stream-id routing (§6).
  `srt-tokio`/`srt-protocol` (v0.4.4, 2024-05) expose caller/listener/rendezvous + AES-CTR +
  TSBPD + a `SocketStatistics` struct + a `stream_id` builder + an `access` acceptor module,
  with **zero native deps** — keeping the default `cargo check` FFI-free and the crate
  `unsafe_code = forbid`. **Honestly de-risked:** its README says *"THIS IS NOT PRODUCTION
  READY"*, it last shipped 2024-05, it does not support SRT < 1.3.0 (HSv4), and the reference
  `srt-c-unittests` "do not pass yet" — so it is **gated behind a feature and validated
  per-endpoint against the FFmpeg/libsrt baseline** before it is trusted in production.
  License is **Apache-2.0 only** (verified — *not* dual MIT-OR-Apache), still compatible with
  Multiview's own source-available license and the LGPL-clean default (we may use and redistribute the
  Apache-2.0 dependency; Multiview's outbound license does not change that).
- **Tier 2 — owned `multiview-srt-sys` libsrt FFI leaf (only-if-required, `srt-libsrt`
  feature).** Built **only when a hard requirement appears** that the other tiers cannot meet:
  **connection bonding** (broadcast/backup — exists *only* in libsrt ≥ 1.5 with
  `ENABLE_BONDING`, absent from FFmpeg **and** srt-tokio — verified), HSv4/legacy-peer interop,
  or proven-at-scale throughput. Follows [ADR-0028](../decisions/ADR-0028.md) exactly —
  resolve-once, RAII handles, **all `unsafe` confined to the leaf** so `multiview-input`/
  `-output` stay `forbid(unsafe_code)` — with the crypto backend chosen as `USE_ENCLIB=
  openssl-evp` **or** `mbedtls` (both Apache-2.0; avoid GnuTLS/Nettle). **Deferred** because it
  pulls cmake + a C++ toolchain + libclang/bindgen + a crypto backend at build time, and the
  only existing FFI crates (`libsrt-sys` 1.4.13, `srt-rs` 0.2.3) are ~6 years stale and
  pre-bonding — pure debt absent a trigger.

Phasing is dependency-ordered and **each phase ships end-to-end** (no "core now, wiring
later"). Phase 0 is mostly wiring already-tested pure code.

---

## 3. Modes matrix — caller / listener / rendezvous × ingest / egress

Connection **mode is orthogonal to data direction** (SRT connections are logically
bidirectional since 1.3); you pick the mode by **who can traverse the firewall/NAT** — i.e.
who accepts inbound UDP — not by who sends. SRT runs over UDP and has **no IANA-registered
port** (verified); the listener port is operator-chosen.

| | **Ingest (we receive)** | **Egress (we send)** |
|---|---|---|
| **Caller** (we initiate) | Pull from a remote listener. **Tier-0 today.** | Push to a known downstream listener (CDN/decoder). **Tier-0 today.** Default `mode=caller` (matches `SrtConfig::default()` and FFmpeg). |
| **Listener** (we accept) | **Contribution aggregation:** many encoders caller-push to one stable port, demuxed by stream-id → one `Source` each. FFmpeg **cannot** (single-connection). **Tier-1.** | Players pull from us; one rendition fanned to N subscribers by stream-id. FFmpeg listener is single-subscriber. **Tier-1.** |
| **Rendezvous** (both initiate) | Fallback when **both** ends are NATed; both bind a mutually-agreed symmetric port and punch through both firewalls. The handshake still assigns Initiator/Responder via the cookie exchange. Fragile — prefer caller↔listener. Tier-0/1. | as ingest. |

Bonding (Tier-2, §8) is **caller/listener only — never rendezvous**. All six cells are
expressible in the typed schema (§10); Tier-0 covers caller-both + single-connection listener
+ rendezvous, and Tier-1 adds the true-multiplexing listener.

---

## 4. Latency / TSBPD timing under invariant #1

SRT's **TSBPD** (Timestamp-Based Packet Delivery) holds each packet in a receive buffer and
releases it at `PktTsbPdTime = TsbPdTimeBase + TsbPdDelay + pkt_ts + Drift`. **This is an
input jitter buffer, not a pacer:** it governs only when `libsrt` hands a packet *up* to
demux/decode — it must **never** drive `out_pts`. The fixed output clock **samples** whatever
the SRT-fed decoder's last-good-frame store holds at each tick (inv #1); a late /
NAK-retransmitted / dropped packet simply becomes a gap the tile state machine (inv #2)
absorbs. **TLPKTDROP** (default `true` in live mode) is the protocol-level peer of
last-good-frame — it drops a packet that misses its delivery slot so the receiver clock never
stalls. **Do not disable it.**

**Config semantics (verified, nuanced):**

- **Negotiated, not requested.** The effective latency per direction is
  `max(local RCVLATENCY, peer's PEERLATENCY)` (the separate `RCVLATENCY`/`PEERLATENCY` knobs
  and the bidirectional model arrived in **SRT 1.3.0**; before that only `LATENCY` existed).
  You **cannot under-buffer** below the peer's demand, so telemetry must surface the
  **negotiated** effective latency (read back after connect), not just the requested value.
- **RTT multiplier is not fixed.** Haivision's "≈ 4 × RTT" rule of thumb holds for a *good*
  link (0.1–0.2 % loss, ≥ 20 ms RTT assumed); the recommended multiplier scales **up toward
  ~20×** as loss rises (below ~3× SRT cannot recover; the verdict corrected the naive
  "fixed 3–4×"). Achieved glass-to-glass ≈ `negotiated_latency + ½·RTT` and degrades
  worse-than-linearly with RTT.
- **Units footgun (verified).** libsrt's **native API is milliseconds** (`LATENCY` default
  120 ms), but FFmpeg's `libsrt` AVOptions are **microseconds** (`latency=200000` ⇒ 200 ms);
  `connect_timeout` stays ms even in FFmpeg. **The seam owns the conversion:** carry
  `latency_ms: u32` in the typed schema, multiply `×1000` at the FFmpeg boundary (fixing the
  `srt.rs:266` bug), pass ms-native to srt-tokio.

Internal time stays **i64 ns / exact rationals, never float** (inv #3). SRT latency is just
one term in the per-source input-latency budget feeding the framestore — then it is *forgotten*
at the output stage, where every PTS/DTS is re-stamped from the tick counter (inv #6).

---

## 5. Encryption + secrets

SRT bulk-encrypts the payload with **AES-CTR** (AES-GCM AEAD was added as a Preview API in
1.5.2, build-gated, requires TSBPD) under a selectable key length **128/192/256** (`pbkeylen` 16/24/32; `0` ⇒
effective AES-128). **Two-tier keys:** the passphrase derives the **KEK** via
`PBKDF2(passphrase, salt)`; a random short-lived **SEK** actually encrypts the media and is
AES-key-wrapped under the KEK and shipped in a **KM (Keying Material)** control message at
connect — **the passphrase itself is never transmitted**. Rekey is **seamless** (even/odd SEK
+ pre-announce; `KMREFRESHRATE` default 2²⁴ packets, `KMPREANNOUNCE` 2¹²). **Enforced
encryption** (default `true`) **rejects** a one-sided or mismatched passphrase — there is no
silent downgrade to cleartext.

- **Passphrase length (verified-corrected).** libsrt enforces **`10..=80` inclusive**
  (`HAICRYPT_SECRET_MAX_SZ = 80`); the FFmpeg wrapper and Haivision product docs say
  `10..=79`. The orphaned `SrtConfig::validate` uses `10..=79` — an **off-by-one** vs the
  library. **Widen to `10..=80`**, and emit a validation *warning* (not error) for an 80-char
  passphrase when the resolved backend is Tier-0 FFmpeg (which caps at 79).
- **Secrets discipline (the repo secret rule).** The passphrase is a high-value, long-lived
  secret (it derives the KEK that unwraps every rotating SEK). **Config must hold only a
  `SourceAuth.secret_ref`** (`op://…` / env / file path — `schema.rs:185`), resolved to
  plaintext **only at socket setup**, zeroized after, **never** a plain `String` in the serde
  struct, **never** in the SRT URL query, and **always** on the tracing redaction list (the
  existing `to_url_redacted` masks it `***` — keep and extend it). gitleaks CI must never see
  it; config-as-code export round-trips the **reference only** (mirroring libsrt's write-only
  `SRTO_PASSPHRASE`: the UI may report `encryption: AES-256` but can never read the passphrase
  back).
- **Isolation (inv #10).** A decrypt failure (`SRT_REJ_BADSECRET`) is just another input
  fault — the tile rides LIVE→STALE→NO_SIGNAL; the engine never awaits a key exchange or rekey.

---

## 6. Stream-id + listener multiplexing + routing

`SRTO_STREAMID` (≤ 512 chars, set by the **caller** pre-connect, read by the **listener** on
the accepted socket) is the **only** per-connection metadata available before payload, and the
**only** way one listener port demultiplexes many callers — libsrt does **not** route by it
for you, and there is **no** port redirection.

**Access-Control "Standard" syntax (verified):** prefix `#!::`, then comma-separated
`key=value` (the `:` selects the comma-separated, no-nesting format). Reserved single-letter
keys: `u` (user / auth name → which passphrase to expect), `r` (resource → the logical
source/tile id), `h` (host), `s` (session), `t` (type: `stream`/`file`/`auth`), `m` (mode:
`request` = receive [**default**] / `publish` = send / `bidirectional`). **Custom keys MUST be
namespaced** (`user_*` / `companyname_*`, e.g. `multiview_tile`) — never invent single letters.

- **Ingest model.** Multiview-as-listener spawns **one `Source` per accepted connection**,
  keyed by stream-id: require `m=publish`, use `r` as the tile/source id, use `u` to select a
  per-user passphrase.
- **Egress model.** Players pull with `m=request` and a stream-id selecting which rendition
  (encode-once-mux-many means several renditions can be offered, inv #7).
- **Isolation maps directly (inv #10).** The accept/reject decision runs in libsrt's receiver
  worker thread (or srt-tokio's `access` acceptor). It **must** be an in-memory `O(1)`/`O(log n)`
  lookup from a **watch-channel snapshot** populated by the control plane — **never** blocking
  on SQLite, the command bus, or any lock an input holds. *A slow accept callback is literal
  ingest back-pressure.* Reject with precise `SRT_REJX` codes: 1401 unauthorized, 1404
  not-found, 1409 conflict (duplicate stream-id), 1402 overload (at capacity), 1400 bad-request
  (unparseable). HSv4 / < 1.3 callers send **no** stream-id — they need a fallback policy
  (reject, or a default bucket).

FFmpeg **cannot** do any of this (single-connection, no `srt_listen_callback`), so listener
multiplexing is a **Tier-1 deliverable**. Routing integrates with
[ADR-0034](../decisions/ADR-0034.md)'s `RoutingTable`/`StreamInventory` (a `StableStreamId`
per accepted stream-id). Adding/removing a stream-id route or rotating a passphrase is a
management change the control plane classifies **Class-1/Class-2** (inv #11).

---

## 7. Statistics → telemetry / health

**Critical verified blocker:** FFmpeg's `libavformat` `libsrt` protocol **never calls
`srt_bstats`/`srt_bistats`** (source grep: no match; a March-2024 ffmpeg-devel patch adding a
`stats` option was **not merged**). So the **entire** statistics catalogue is **unreachable**
through the Tier-0 path the code uses today. This — not raw transport — is the primary
justification for owning a deeper path (Tier-1 srt-tokio `SocketStatistics`, or Tier-2 FFI
`srt_bistats`).

**Stats model.** Each accumulated metric has a monotonic `…Total` (→ Prometheus **counter**)
and an interval field (→ rate/health window). Poll the Totals with **`clear=0`** and let
`rate()` difference them — **never clear** (it corrupts other readers' windows); use the
instantaneous form for live buffer **gauges**. The canonical call is **`srt_bistats()`**
(`srt_bstats` is the older name).

**Role-aware (emit only the valid fields, tag `role=ingest|egress`):**
- **Ingest (we receive):** `pktRcvLoss`, `pktRcvDrop`, `pktRcvBelated`, `msRcvBuf` — the
  direct glitch predictors for the sampled tile.
- **Egress (we send):** `pktSndDrop`, `pktRetrans`, `pktFlightSize`, `msSndBuf`,
  `mbpsBandwidth`.
- `msRTT` is a **smoothed** RTT (there is **no** variance field in the struct — do not promise
  `rtt_variance`). `mbpsBandwidth` is a **noisy** packet-pair estimate — **advisory only,
  never gate admission/degradation (inv #9) on it**; decide on measured send/recv rate +
  loss % + drop/belated counts + RTT.

**Wiring (verified seams).** Reuse `multiview_telemetry`'s `MetricsRegistry`
`counter()`/`gauge()`/`histogram()` + `Labels`; map to `multiview_srt_pkt_*_total` counters
and `multiview_srt_rtt_ms` / `_recv_rate_mbps` / `_rcv_buf_ms` / `_flow_window_pkts` gauges.

**Health (not alarms).** SRT link degradation is **operator-actionable infrastructure
health**, not an A/V *content* alarm (Black/Freeze/Silence). It belongs in the
[ADR-0035](../decisions/ADR-0035.md) **`WarningCode` catalog** (`event.rs:284`), **not** a new
`AlarmKind`. Add `srt-link-packet-loss-sustained`, `srt-link-latency-spike`,
`srt-connection-unstable`, `srt-rtt-ceiling-hit`, emitted as
`Event::HealthWarningRaised`/`Cleared` (`event.rs:573`/`577`) carrying
`HealthWarning { code, severity, subsystem: "srt", message, remediation, since, active }`
(`event.rs:307`) on `Topic::Alerts` via the **same drop-oldest publisher** as `SystemMetrics`
(inv #10), with **hysteresis** (latch + dwell — no flapping, inv #9). Read-only
`GET /api/v1/health`; **never `/readyz`** (a degraded SRT link must not restart-loop the
container).

**Poller.** One detached tokio task per link, ~1–2 Hz, publishing a conflated latest-value
snapshot via a `watch` channel — the engine/compositor never read it and never await it.

---

## 8. Bonding (later — Tier-2 only)

Connection **bonding = socket groups**, **stabilized in libsrt v1.5.0** (2022-06-15;
`ENABLE_BONDING` renamed from `ENABLE_EXPERIMENTAL_BONDING`; first appeared *experimentally* in
the 1.4.x line — verdict-corrected). Two usable group types:

- **Broadcast** — send the same payload on every member link, dedupe at the receiver. The
  ST 2022-7-style hitless redundancy (≈ 2× bandwidth, zero added latency).
- **Backup / main-backup** — one active link + weighted hot standby; on failure it switches
  over and replays since the last ACK.

**Balancing** is under development and was **removed from 1.5.0** — do **not** design around it.
Bonding is **caller/listener only (not rendezvous)**.

**Why this is Tier-2:** it is **compile-gated** (`ENABLE_BONDING`, off by default — the group
symbols exist but error if not compiled in, so a runtime capability probe is required); FFmpeg
has **no** group API (verified — single-socket only); srt-tokio has **no** socket groups; and
the only FFI crates are pre-bonding (1.4.x, stale). So bonding strictly **requires an owned
libsrt ≥ 1.5 FFI binding** (`multiview-srt-sys`, [ADR-0028](../decisions/ADR-0028.md) pattern).
libsrt does **not** auto-restore broken members, so the supervised-reconnect ladder (inv #2)
must extend **down into the group** (a per-member watcher polling `SRT_SOCKGROUPDATA`, re-adding
links).

**Multiview fit (when built):** broadcast bonding for bulletproof ingest/egress over lossy or
diverse paths composes cleanly with inv #1/#7/#10 — the bonded receiver hands **one**
de-duplicated stream to the framestore; the group send is wrapped in a bounded drop-oldest
queue the engine never awaits. Adding/removing a member mid-stream is **Class-1**; switching
single↔bonded or changing the group type is **Class-2** (inv #11). Per-member state (active
link, weights, switchover events) surfaces as conflated telemetry. **Explicitly out of scope
this cycle.** **RIST (VSF TR-06)** is likewise a future standards-track option, not a parallel
deliverable; SRT 1.5 bonding covers the multi-path redundancy that was RIST's main
differentiator. The decision criterion that would **reopen** RIST: a customer requiring
ST 2022-7 hardware interop the SRT ecosystem cannot meet.

---

## 9. Resilience + isolation (invariants #1 / #2 / #10)

SRT integrates with the **existing** resilience seams with **no new engine machinery**.

- **Inv #1 (output-clock).** SRT receive (TSBPD) is a sampled input jitter buffer; the output
  clock never blocks on `srt_recv` or the next TSBPD release.
- **Inv #2 (last-good-frame + state machine).** An SRT source connecting / dropping /
  reconnecting / decrypt-failing rides the existing `TileStore` LIVE→STALE→RECONNECTING→
  NO_SIGNAL ladder (framestore `state.rs` classify + `liveness.rs` `PacketLiveness`), keyed on
  **freshness, not socket liveness** — a *connected-but-dropping* SRT link (rising
  `pktRcvDrop`/`pktRcvBelated`) must transition to STALE **even while "connected"**.
- **Reconnect.** Reuse `multiview_input::reconnect::Backoff` (`reconnect.rs:53`, capped
  exponential + jitter) and `multiview_engine::supervisor::RestartPolicy::backoff`
  (`supervisor.rs:103`). SRT timeouts feed it: `PEERIDLETIMEO` (~5 s) ⇒ source-dead trigger;
  `CONNTIMEO` (~3 s) ⇒ connect-attempt bound.
- **Inv #10 (isolation).** Every SRT-outside→engine path is bounded drop-oldest and the engine
  never awaits it. The egress sink reuses `PacketMuxSink` + `consumer_main`'s
  `fan_packets`/`send_bounded` and the `SINK_WEDGE_GRACE` (2 s, `pipeline.rs:138`)
  detach-on-grace, so a slow/lossy SRT peer (full send buffer) is **dropped, never back-
  pressuring** the encoder; the stats poller publishes via a `watch` channel; the listener
  accept-callback does only in-memory lookups.

A **chaos gate** (new blocking test, §11) must prove: kill/lag/loss an SRT ingest **and** an
SRT egress peer while asserting the output clock keeps emitting one frame per tick and no
engine queue grows — the same chaos discipline inv #10 already mandates for any engine→outside
channel.

---

## 10. Crate / feature / config surface (backward-compatible)

**Crates.** `multiview-config` (typed schema + validation), `multiview-input` (the existing
`SrtConfig` + Tier-1 listener/ingest + stats poll), `multiview-output` (push + listener egress),
`multiview-ffmpeg` (Tier-0 `srt://` open — already), and an optional new `multiview-srt-sys`
(Tier-2 FFI leaf, only-if-bonding). **No new crate for Phases 0–1.**

**Features** (precedent: `ndi`/`ndi-bindings`, `webrtc`/`webrtc-native`):
- The SRT **byte path stays under the existing default `ffmpeg` feature** — **not**
  license-escalating (MPL-2.0, no GPL; `deny.toml` already allows it).
- Add `srt` (off-by-default) pulling `srt-tokio` for Tier-1 stats + listener.
- Add `srt-libsrt` (a further off-by-default gate) pulling `multiview-srt-sys` for Tier-2
  bonding/FFI.

**Schema extension — must stay byte-compatible with today's `SourceKind::Srt { url }` and
`Output::Srt { …, url, … }`.** Add **optional**,
`#[serde(default, skip_serializing_if = "Option::is_none")]` fields so an existing
`{ kind: "srt", url: "srt://…" }` config parses unchanged (**no schema version bump** — purely
additive). Add `srt: Option<SrtOptions>` to **both** `SourceKind::Srt` and `Output::Srt`,
**mirroring how `Rtsp` already carries `Option<RtspOptions>`** (`schema.rs:245`). `SrtOptions`
is the serializable projection of the existing `SrtConfig`:

```
SrtOptions {
  mode: SrtMode,                 // internally tagged, default `caller`
  latency_ms: Option<u32>,
  encryption: Option<SrtEncryption { key_length: KeyLength, passphrase_ref: String }>,
  stream_id: Option<String>,     // validated `#!::` Access-Control grammar
  listener: bool,                // default false
  oheadbw_pct: Option<u32>,      // default 25 (maxbw=0 ⇒ relative to input rate)
  maxbw: Option<i64>,
  peer_idle_timeout_ms, conn_timeout_ms: Option<u32>,
  backend: Option<SrtBackend>,   // { Ffmpeg | Native }; default auto-select
}
```

All unions **internally tagged** (`#[serde(tag = …)]`), **never `untagged`** (repo rule). When
`srt` is absent, behaviour is exactly today's `{ url }` passthrough. **Validation**
(`multiview-config`, at admission, **not** the hot path): relocate/reuse `SrtConfig::validate`
(passphrase **widened to `10..=80`**), check `pbkeylen ∈ {0,16,24,32}`, latency range,
stream-id ≤ 512 B + `#!::` parse, and **reject** FEC/bonding configs whose latency budget
cannot cover them. The passphrase is a `secret_ref` (never inline), resolved at socket setup.

---

## 11. Test strategy (no special hardware)

All of this is testable on commodity CI with **no SRT gear** — the pure seams + FFmpeg loopback
+ (later) srt-tokio loopback cover it; hardware interop is a separate self-hosted tier.

- **Pure (default, no features).** `SrtConfig`/`SrtOptions` round-trip + validate **property
  tests** (`proptest`): passphrase `10..=80` boundary (assert **79 *and* 80** accepted, 9/81
  rejected — catches the off-by-one), `pbkeylen` domain, **latency µs-vs-ms** conversion (a
  golden table asserting `×1000` at the FFmpeg boundary), stream-id `#!::` Access-Control parse
  (`u`/`r`/`m`/`t`, reject single-letter custom keys, reject unparseable), percent-encoding of
  `#`/`&`/`=`. **Backward-compat:** a legacy `{ kind: "srt", url }` (no `srt` block) still
  deserialises and round-trips byte-identical (serde golden). Extend the existing
  `PushProtocol::Srt.muxer_name() == "mpegts"` contract test (`push_and_gpl.rs:163`).
- **Seam (state machines, fake transport — mirror the WHEP seam tests).** `SrtTransport`
  accept → `SessionState` Created→Connecting→Connected→Closed; reject illegal transitions; the
  stream-id acceptor routes `m=publish` to a tile, rejects `m=request` on ingest, rejects a
  duplicate stream-id with **1409**, and applies the configured fallback for empty/HSv4
  stream-ids; `SampleSink`/`SampleFeed` bounded drop-oldest never blocks.
- **Stats.** Feed a recorded `SocketStatistics`/`SRT_TRACEBSTATS` fixture through the mapper;
  assert Total→counter, instantaneous→gauge, role-masking (ingest hides sender fields), and
  health-warning **hysteresis** (loss %/RTT thresholds latch + dwell, don't flap).
- **Loopback (feature-gated, still no special hardware).** Tier-0: `ffmpeg` feeds a known TS
  over `srt://127.0.0.1:PORT?mode=caller` to a co-process `…?mode=listener`; assert frames
  arrive + PTS unwrap. Tier-1: a srt-tokio listener accepts two callers with distinct
  stream-ids; assert two `Source`s spawned + routed.
- **Chaos gate (inv #10, blocking).** A harness that drops/lags/loss-injects an SRT ingest
  **and** an SRT egress peer (kill the loopback co-process / inject a lossy UDP relay) while
  asserting **(1)** the output clock emits one frame/tick throughout, **(2)** the tile rides
  LIVE→STALE→RECONNECTING→NO_SIGNAL, **(3)** no engine queue grows, **(4)** a wedged SRT egress
  sink is **detached at `SINK_WEDGE_GRACE`, not awaited**.
- **Interop (self-hosted, NOT CI-blocking).** Validate srt-tokio against **real reference-libsrt
  senders/receivers** per endpoint before any Tier-1 path is trusted in production — its "NOT
  PRODUCTION READY" status mandates this. (Open question: confirm the self-hosted tier has
  SRT-capable tooling, or have CI shell out to the FFmpeg `libsrt` build as the reference peer.)

---

## 12. Open questions

- **FFmpeg passphrase cap vs schema.** Tier-0 caps the passphrase at 79; the typed schema
  accepts `10..=80`. *Proposed reversible default:* accept `10..=80`, emit a validation
  **warning** (not error) when `backend = Ffmpeg` and `len == 80`, and surface a connect
  failure as a `HealthWarning`. Decide if a hard reject is preferred.
- **Backend selection.** Operator-explicit `backend: Ffmpeg|Native` from day one, or
  auto-select (FFmpeg for caller, srt-tokio for listener, since FFmpeg can't multiplex)?
  *Proposed:* auto-select with an optional override; confirm whether the override is exposed in
  the WebUI this cycle.
- **HSv4 / SRT < 1.3.0 legacy peers** on the srt-tokio listener (which cannot handshake them):
  silent reject, a `HealthWarning`, or a documented Tier-2 requirement? *Proposed:* reject +
  `srt-legacy-peer-unsupported`; confirm whether legacy interop is a real requirement that
  pulls SRT-11 forward.
- **FEC (`SRTO_PACKETFILTER`)** this cycle? It auto-raises the latency budget by
  `N = rows·(cols−1)+2` packets. *Proposed:* defer; validate-reject FEC configs until
  implemented.
- **Duplicate-stream-id policy** on a listener: reject (1409) vs last-writer-wins replace?
  *Proposed reversible default:* reject 1409; make it a live-reconfigurable field.
- **AES-GCM** (`CRYPTOMODE=2`, 1.5.2+ Preview API, build-gated, requires TSBPD) is reachable only via Tier-2
  FFI; `SrtEncryption` does not yet model a cipher-mode choice. If AEAD integrity becomes a
  requirement, add a `CryptoMode` field.
- **Counter-wrap / rekey analysis** (deferred to before Tier-2): the 32-bit µs TSBPD timestamp
  wraps ≈ every 71.6 min; compute, for a representative multiview egress bitrate, how often
  `KMREFRESHRATE` rekey fires and whether the wrap window interacts with `WrapBits::Mpeg33` PTS
  handling.

---

## 13. Citations

- **Protocol / latency / timing:** SRT RFC Internet-Draft `draft-sharabayko-srt` (TSBPD,
  AES-CTR/GCM, KM); Haivision/srt `docs/features/latency.md`, `docs/API/API-socket-options.md`
  (`LATENCY`/`RCVLATENCY`/`PEERLATENCY` ms, default 120; `TRANSTYPE`; `PAYLOADSIZE` 1316 =
  7×188 = `SRT_LIVE_DEF_PLSIZE` in `srtcore/srt.h`, max 1456; `OHEADBW` 25 % / `MAXBW`=0
  relative; `TLPKTDROP`; `PEERIDLETIMEO` 5000 / `CONNTIMEO` 3000); srt-cookbook
  latency-negotiation (max-of-pair). [streaming-gotchas](streaming-gotchas.md),
  [timing-architecture](timing-architecture.md), [ADR-T003](../decisions/ADR-T003.md).
- **Encryption:** Haivision `docs/features/encryption.md` (KEK/SEK, PBKDF2, odd/even rekey);
  `srtcore/socketconfig.cpp` + `haicrypt/haicrypt.h` (passphrase `10..=80`,
  `HAICRYPT_SECRET_MAX_SZ = 80`); `ENFORCEDENCRYPTION` default true.
- **Stream-id / Access Control:** Haivision `docs/features/access-control.md` (`#!::`,
  `u/r/h/s/t/m`, `m=request` default, listen-callback timing); `docs/API/rejection-codes.md`
  (`SRT_REJX_*` 1400/1401/1402/1404/1409).
- **Statistics:** Haivision `docs/API/statistics.md` (`srt_bstats`/`srt_bistats`,
  `CBytePerfMon`, Total-vs-interval, sender/receiver split).
- **Bonding:** Haivision `docs/features/socket-groups.md`, `bonding-main-backup.md`,
  `bonding-quick-start.md`; release **v1.5.0** (`ENABLE_BONDING`, balancing removed) + **v1.4.4**
  (experimental).
- **FFmpeg:** `libavformat/libsrt.c` (master — ~40 AVOptions, **no** `srt_bstats`/`srt_bistats`,
  **no** group API, `srt_listen(fd,1)` + single `srt_accept`, latency in µs);
  ffmpeg.org/ffmpeg-protocols.html; FFmpeg `configure` (libsrt in `EXTERNAL_LIBRARY_LIST`).
  [ffmpeg-strategy](ffmpeg-strategy.md), [ADR-0031](../decisions/ADR-0031.md).
- **Rust ecosystem:** russelltg/srt-rs README ("NOT PRODUCTION READY"; modes/AES/TSBPD; no
  bonding; HSv4 issue #146); crates.io `srt-tokio`/`srt-protocol` 0.4.4 (**Apache-2.0**);
  docs.rs `srt-tokio` `SocketStatistics` + `access` module; `libsrt-sys` 1.4.13 / `srt-rs`
  0.2.3 (stale).
- **License:** Haivision/srt `LICENSE` (**MPL-2.0**); mozilla.org MPL-2.0 FAQ (file-level
  copyleft, Larger Work).
- **In-repo:** `schema.rs:175/245/269/635`, `sink.rs:1039/1233`, `demux.rs:191`,
  `pipeline.rs:138/2307/3235/3545/4277`, `capability.rs:31/98`, `lib.rs:320/520`,
  `crates/multiview-input/src/srt.rs` (full) + `tests/srt_config.rs`,
  `preview/whep/transport.rs:61/153/300/310`, `reconnect.rs:53`, `supervisor.rs:103`,
  `events/event.rs:284/307/573/577`, `multiview-ndi-sys` + [ADR-0028](../decisions/ADR-0028.md),
  [ADR-0034](../decisions/ADR-0034.md), [ADR-0035](../decisions/ADR-0035.md).
