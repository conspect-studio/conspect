# AES67 / SMPTE ST 2110-30 audio I/O — delivery design brief

**Status:** design (2026-06-07). Builds on and does **not** duplicate the predecessor
research. Drives [ADR-0033](../decisions/ADR-0033.md);
consistent with [ADR-T010](../decisions/ADR-T010.md).
**Question:** what is the implementation-ready architecture for **AES67 /
SMPTE ST 2110-30 linear-PCM audio over IP — send *and* receive** — so Multiview
interoperates with broadcast audio-over-IP plants, open-interop-first, pure-Rust,
LGPL-clean, no SDK?

> **Scope split.** [ADR-T010](../decisions/ADR-T010.md) decided *why* AES67/ST 2110-30 is
> the interop path (it is Audinate's own licence-free bridge to Dante; native Dante
> is closed and is NOT supported). **This brief
> does not re-argue that** — it adds the send/receive architecture, the clocking
> reconciliation, the discovery tiers, the resilience model, and the
> crate/feature/config surface, all grounded in the as-built code. Native Dante is
> out of scope here.

---

## 0. Headline

AES67 is **open** (RTP + L16/L24 PCM + SDP + SAP + PTPv2 — RFC/AES/SMPTE only), so
unlike NDI there is **no SDK and no runtime-load gate**: it ships in the off-by-default
but pure-Rust, `unsafe_code = forbid`, LGPL-clean `st2110` feature. Multiview already
owns the hard receive primitives (the L16/L24 depacketizer, the RTP parser, the
ST 2022-7 merge engine, the PTP servo, the lock-free `AudioStore`, an NMOS IS-04/IS-05
node). The genuinely-new work is the **sender**, the **f32↔Lk codec**, an **audio
`FrameProducer`**, **AES67 SDP build/parse**, **SAP discovery**, **absolute-placement
on the store**, and the **config/control surface**. Clocking is already decided by
[ADR-T010](../decisions/ADR-T010.md) §3: a **PTPv2-follower media-clock reference, never
a pacer** — the output tick stays the sole pacer (invariant #1).

---

## 1. As-built vs missing (verified against the tree)

**Reuse verbatim — already built and property-tested (no change):**

| Primitive | Location | What it does |
|---|---|---|
| L16/L24 depacketizer | `multiview-input/src/st2110/v30.rs` | Big-endian wire bytes → sign-extended `i32` per `(group, channel)`; L16 via `i16::from_be_bytes`, L24 via `i32::from_be_bytes([hi,mid,lo,0]) >> 8`; whole-sample-group enforcement (`V30Error::PartialGroup`) satisfies RFC 3190 (all channels of one sampling instant in one packet). |
| RTP header parse | `multiview-input/src/st2110/rtp.rs` | 12-byte RFC 3550 fixed header; sibling free functions `seq_after` (RFC 1982 serial compare) / `seq_distance` (wrapping `u16`). |
| Transport / NIC-free seam | `multiview-input/src/st2110/transport.rs` | `RtpReceiver` / `ChannelPacketSource` / `PacketSource`; bounded queue; non-blocking `poll_packet` via `try_recv` (inv #1/#10). |
| ST 2022-7 merge | `multiview-input/src/st2022_7.rs` + `transport.rs` `DualPathPacketSource` | `HitlessReconstructor<P>` keyed purely on unwrapped 16-bit RTP seq, essence-agnostic; opaque payload merges *upstream* of depacketize. |
| PTP servo + reference | `multiview-engine/src/ptp.rs` | Pure-value `PtpServo` (integer ns/ppb, step-vs-slew, outlier guard), `ReferenceTracker` with `LockState {Freerun, Acquiring, Locked, Holdover}`; reads PHC via `rustix` (`ptp` feature) or disciplined system clock (`ntp` feature). Servo disciplines a **separate** estimate beside the tick — it can neither stall nor speed the output clock. |
| Program audio bus | `multiview-audio/src/program.rs` `ProgramBus::tick()` | Returns interleaved frame-major `f32` `AudioBlock` (`format.rs` `[l0,r0,l1,r1,…]`) — already the exact channel interleave AES67 needs; only per-sample encoding differs. |
| Variable-tick → fixed FIFO | `multiview-output/src/sink.rs` `AudioState` | Integer repacketizer: rebuffers ~1601/1920-sample ticks into AAC's 1024 `frame_size`. **Pure rate-*preserving*** (input rate == output rate). Reusable for sample→packet regrouping **only**; it is *not* a clock-domain reconciler. |
| Sample-budget accumulator | `multiview-audio/src/cadence.rs` `SampleClock` | Exact integer-rational per-tick budget (`carry`/`step_num`/`step_den`, `% step_den`) — never float (inv #3). The **pattern** is reusable for non-integer ptimes; the **instance** is tick-keyed and must not back a PTP-absolute timestamp (§4). |
| Lock-free store | `multiview-audio/src/store.rs` `AudioStore` | Wait-free single-slot ring; `read(frames)` always returns exactly `frames`, silence-filling unwritten/evicted spans (never a gap, never blocks). |
| NMOS node | `multiview-control/src/nmos/{is04,is05,mod,transport}.rs` | Served Node API (`/x-nmos/node/v1.3/*`, `/x-nmos/connection/v1.1/*`), staged/active + scheduled-TAI activation (absolute + relative), property- and integration-tested. |

**Genuinely missing (this design):**

1. **AES67 RTP sender** — grep confirms **zero** `RtpSender`/transmit in the tree; only NDI `send_*` and the AAC mux exist. New `Aes67Packetizer` (`multiview-output`).
2. **f32↔Lk codec** — the encode inverse of `v30.rs` (receive currently only decodes).
3. **Audio `FrameProducer`** — the existing `FrameProducer`/`St2110Producer` are video-shaped (`ProducedFrame` carries pixels/`PixelFormat`/w/h, raster `FrameAssembler`, `TileStore<StoredFrame>`). The audio producer is a **parallel** path (produced type `AudioBlock`, target `AudioStore`), not a reuse of `St2110Producer`.
4. **`AudioStore::publish_at(rtp_ts)`** — today only `publish()` appends at the write head; a network receiver needs absolute placement to honour gaps/reorder with silence-fill (its own TDD slice).
5. **AES67-audio SDP build + parse** — the existing `parse_sdp_transport` (`is05.rs:340`) handles only `c=`/`m=video`/`a=source-filter`; no `m=audio`, no `a=rtpmap` L16/L24, no `a=ptime`/`a=ts-refclk`/`a=mediaclk`, **no generator**.
6. **SAP** announce/listen — zero `sap`/`9875` hits.
7. **Fractional ppm resampler** — the clock-domain reconciler. **Does not exist** (the only resampler, `multiview-ffmpeg/src/resample.rs`, is a fixed-ratio `swr` wrapper, `ffmpeg`-gated, no compensation knob).
8. **`AlarmKind::ReferenceLoss`** — absent from `alarm.rs` today.
9. **Config + control surface** — `SourceKind`/`Output` variants, discovery endpoint.

The receive-wire layer (items reused above) is the most complete part; the
**estimate "~80%" applies to that layer only** — the new audio producer, `publish_at`,
SDP, SAP, the sender, and the resampler are real new code.

---

## 2. Send architecture — `Aes67Packetizer` (`multiview-output`)

Genuine greenfield. Fed from the **same off-hot-path bake consumer** that already calls
`ProgramBus::tick()` and fans AAC packets (`pipeline.rs` `consumer_main`), so a NIC
stall back-pressures only the sink, never the engine (inv #1/#10, §5).

1. **Source.** `ProgramBus::tick()`'s interleaved frame-major `f32` `AudioBlock` — already AES67's channel order; only per-sample encoding changes.
2. **Regroup.** Rebuffer the variable per-tick block into **fixed groups-per-packet** (48 for 1 ms @ 48 kHz) via the proven integer FIFO **pattern** (`AudioState`). This is sample→packet regrouping, rate-*preserving* — **not** the drift reconciler (§4).
3. **RTP header.** Version=2, no padding/ext/CSRC, **dynamic PT 96–127**, random constant SSRC, `seq = seq.wrapping_add(1)`, **`marker = 0` on *every* packet** (see below), `timestamp` advanced per §4.
4. **Encode `f32` → big-endian `Lk`** (exact inverse of `v30.rs`):
   - **L16:** `clamp(x, -1.0, 1.0) · 32767` → `i16` → `to_be_bytes()`. Symmetric, cannot overflow.
   - **L24:** `clamp(x, -1.0, 1.0) · 8388607.0` → `i32`, then write the **high 3 bytes** of `(v << 8)` big-endian. **Scale by `8388607`, not `8388608`** — `8388608 = 2^23` makes a full-scale positive sample wrap to most-negative (a full-scale positive→negative click). Round-trips with `V30Payload::parse`'s `widened >> 8` decode.
5. **Transmit.** `UdpSocket::send_to` a `239.x/16` multicast group, off the engine hot loop, bounded drop-oldest.

**Marker bit (normative correction).** AES67/ST 2110-30 are constant-bitrate,
**continuous, no silence suppression** — and Multiview's output clock (inv #1) emits one
frame per tick forever and never gaps the stream. RFC 3551 is verbatim: *"Applications
without silence suppression MUST set the marker bit to zero."* The SHOULD-set-M-on-first
rule applies **only** to talkspurt/silence-suppression senders, which Multiview is not.
So **M = 0 on every packet, including the first.** (The "marker on first/after-discontinuity"
idea is dropped; it is a MUST violation conformance analyzers flag.)

**Conformance: ST 2110-30 Class A first** — 48 kHz, L16/L24, 1–8 ch, **1 ms ptime =
48 sample-groups/packet** (stereo L24 payload = 48·2·3 = **288 bytes**). Class A is the
**mandatory *receiver* baseline** of ST 2110-30 and therefore the **interop-maximizing
*sender* target** (any conformant receiver decodes it) — senders are not *obligated* to
Class A, they choose it. It is also the Dante-AES67 interop clamp
([ADR-T010](../decisions/ADR-T010.md)) and covers the
stereo/5.1 layouts `multiview-audio` already models. ptime 250 µs/125 µs (Class B/C) is
**additive config**, not first cut. ST 2110-30 also defines extended levels AX/BX/CX for
AES3/2110-31 mixing — **out of scope** for the PCM-only first cut.

**The pure encode core round-trips against `V30Payload::parse` in-memory with no NIC** —
the same NIC-free seam `transport.rs` documents for receive.

---

## 3. Receive architecture — `Aes67AudioProducer` (`multiview-input/src/st2110/`)

Audio analogue of the video-shaped `St2110Producer`, new code paralleling the video path:

1. Pull RTP via the existing `RtpReceiver`/`ChannelPacketSource` (bounded, never back-pressures the wire — inv #10).
2. Parse with `RtpPacket::parse`; depacketize with `V30Payload`.
3. Convert `i32 → f32`: **L16** `v / 32768`, **L24** `v / 8388608`. Build an `AudioBlock`.
4. **Place absolutely** via the new `AudioStore::publish_at(frame)`: map the RTP media-clock timestamp to an `AudioStore` absolute frame so reorder/loss → silence-fill at the right frame (not a click). `read()` already silence-fills exactly `frames`.

The reused `St2110Packet.timestamp` is documented as a **90 kHz video** timestamp; the
audio producer must interpret the same opaque bytes as the **48 kHz audio media clock**
— the payload is essence-agnostic, only the rate-mapping differs. The producer must **not**
inherit the 90 kHz assumption.

**Honour the signalled `a=mediaclk` offset.** ST 2110-30 forces RTP-stream-clock offset
0, but AES67/RAVENNA may use a random RFC-3550 offset. Parse `a=mediaclk:direct=<offset>`
from the SDP and **subtract** it before mapping RTP-ts → absolute frame; default to 0 only
when ST 2110-30-strict is asserted.

**Receive jitter = configurable `link_offset`**, realized as the `AudioStore`
`capacity_frames` floor plus the read cursor's lead over the write head — no new buffer
primitive.

---

## 4. Clocking — PTP media-clock reference vs the output tick (the hard part)

**Decided, not new.** [ADR-T010](../decisions/ADR-T010.md) §3 states it verbatim:
*"be a PTPv2 follower … and use it as the media-clock reference, never as a pacer — the
output tick stays the sole pacer (invariant #1)."* This is
[timing-architecture](timing-architecture.md)'s **Layer A** (monotonic pacer) / **Layer B**
(PTP reference, slew-not-step, holdover→free-run) / **Layer C** (sources sampled, never
pacing). PTP carries rate+phase+epoch but disciplines a **separate** estimate beside the
output clock, never in front of it. Inv #1 holds because the tick count never changes —
only the sample↔wall-time mapping the sender/receiver consult.

**Build on the existing `PtpServo`/`ReferenceTracker` — do not write a new servo, and do
not author/transmit PTP Sync/Follow-Up/Delay messages.** Rely on **external linuxptp**
(`ptp4l` reads the PHC; `phc2sys` disciplines `CLOCK_REALTIME`) as an operational
dependency. Multiview reads `/dev/ptpN` directly (`ptp` feature, safe `rustix`) or the
externally-disciplined system clock (`ntp` feature, `adjtimex` `STA_*` via
`multiview-ntpsys`, arbitrated by `sysref.rs::ReferenceSelector`). **No
`multiview-ptp-sys`.**

### 4.1 SEND timestamp (PTP-anchored absolute, integer-exact)

RTP `timestamp` = media-clock samples since the IEEE-1588 / SMPTE-ST-2059 **1970 TAI
epoch**, mod 2³², clock-rate = sample-rate (48000), offset 0 (`a=mediaclk:direct=0`,
ST 2110-30-strict / RFC 7273).

**Two corrections over the naive form** (`floor(TAI_ns · 48000 / 1e9) mod 2^32`):

- **Decouple from `SampleClock`.** The engine pacer is `CLOCK_MONOTONIC`/`Instant`-backed (`clock.rs` `MonotonicTimeSource`), **not** PTP/TAI; the AES67 media clock every receiver locks to is PTP-disciplined TAI. These are two oscillators that drift. Stamping from the tick-keyed `SampleClock` accumulator walks the timestamps away from receivers' jitter-buffer expectations and from every other ST 2110 sender — defeating the media clock. **Re-anchor** per packet (or periodically) from the PTP-disciplined TAI time read at emission (`ptp.rs` carries i64-ns offsets; `timespec_to_ns` is the seam), not free-accumulating from a start anchor.
- **No u64 overflow.** Mid-2026 `TAI_ns ≈ 1.78e18`; `· 48000 ≈ 8.5e22` is ~4600× over `u64::MAX`. Compute in `u128`, or the algebraically-identical, bit-identical **seconds/nanoseconds split** that stays in `u64`:
  ```
  rtp_ts = ((secs as u128) * 48000
            + (nanos as u128 * 48000) / 1_000_000_000) as u32   // wrapping = mod 2^32
  ```

- **Per-packet increment** by groups-per-packet with `wrapping_add` (Class A 1 ms @ 48 kHz = exactly **+48**, integer, no accumulator). Only introduce a remainder accumulator — the `SampleClock` **pattern**, freshly instantiated, not the engine's tick-keyed instance — when a non-integer ptime is supported.

A property test asserts the `u128` form equals the `u64` seconds/nanoseconds split across
the full nanosecond range; a loopback test stamps a sender from a fixed PTP time and
round-trips to the expected receiver frame for a non-zero offset.

### 4.2 The boundary resampler (net-new, soak-gated)

A continuous **soft fractional resampler** (driven by the **measured master/PTP ppm
offset** — the [ADR-T006](../decisions/ADR-T006.md)/NDI-FrameSync "dynamic resample, never
drop/dup" mechanism, the audio analogue of the video frame-synchroniser) absorbs the
monotonic-tick-vs-PTP-media-clock drift at the boundary, **both directions**.

Honest scoping (this is *not* "proven, same code path"):
- **It does not exist today.** The intent unifies cleanly with the existing `LockState`, but the corrector is net-new.
- **Pure-Rust requirement.** Adopt `rubato` (or `libsoxr`) — **not** the `ffmpeg`-gated `multiview-ffmpeg::Resampler` — to honour the `st2110` feature's pure-Rust / `unsafe_code = forbid` posture.
- **Acceptance gate.** It inherits [ADR-T006](../decisions/ADR-T006.md)'s **72 h soak + EBU R37 window**; the correction must be **driven from the measured PTP offset** (a fixed-ratio resampler with `async=1` is *not* a multi-hour drift fix — ~100 ms accumulates).
- **Quantitative locked-vs-freerun.** When `LockState::Locked`, sender/receiver/output share the disciplined rate (ppm≈0) and the FIFO alone suffices — the resampler is **identity**. Only **Freerun** (the commodity default, no grandmaster) and any receiver whose grandmaster differs from Multiview's reference exercise the VRC path. Test it by **injecting a deliberate ppm offset** and asserting zero output gaps + zero audible drop/dup over a long run.
- **Receive-side placement asymmetry.** As-built `AudioStore.read()` is a drop/dup mechanism (silence-fill underrun, evict-oldest overrun). To reach "resample never drop/dup" on **receive**, the resampler sits **between the depacketizer and `publish_at()`**, not at the store's read cursor.

**Option (b) free-run+resample is not a competing design — it is the graceful
degradation of (a)** when no grandmaster is present (`LockState::Freerun`): same code
path, the reference estimate just reports undisciplined and the resampler + AES67 jitter
buffers absorb the drift.

### 4.3 TAI-UTC offset and leap seconds

Source `currentUtcOffset` (37 s today) **dynamically** from the PHC-vs-`REALTIME` delta or
the grandmaster Announce — **never hard-code** (verify against the live grandmaster). On a
leap event **SLEW (never STEP)** the media clock to preserve RTP-ts continuity.

**Platform reality.** Linux = facility-grade send+recv (real PTP NICs + linuxptp).
macOS has no stable public PHC API → `RealPhcSource` returns `Unsupported`; AES67 falls
back to the `ntp`/system-clock reference (lower accuracy). **Gate ST 2110-30-strict
conformance (which assumes a PTP-locked media clock) to PTP-capable Linux hosts**; macOS
is receive-tolerant / free-run-send.

---

## 5. Resilience and isolation

**Send never back-pressures the engine (inv #1/#10).** The sender is a SINK fed from the
off-hot-path bake consumer (`pipeline.rs` `consumer_main`, where `ProgramBus::tick()`
already runs), behind a bounded drop-oldest queue — identical posture to
`PacketMuxSink`/`ProgramEncoder` ("run off the engine hot path; a slow one paces only its
own consumer, never the engine"). The engine never `.await`s it; `UdpSocket::send_to` is
never called from the output-clock loop; a NIC stall overflows the queue (drop-oldest) and
the engine is unaffected. Include the send queue in the `SINK_WEDGE_GRACE` detach path so
a wedged NIC send is reaped on teardown. **Chaos/soak test:** wedge the AES67 socket
(full buffer / blackhole multicast) and assert the output clock keeps ticking and drop
counters increment.

**Receive never back-pressures the wire (inv #10).** The engine pulls non-blocking
(`poll_packet` → `try_recv`, yields on Empty/Disconnected); the async receive task
discards rather than awaits on a full channel. **Known defect to fix before reuse:**
`channel_bridge` (`transport.rs:172-180`) is documented "drop-oldest" but **drops the
NEWEST** on Full (it discards the just-received unit; never drains+resends). For audio,
drop-newest keeps stale samples over fresh ones — the worse choice. **Fix to genuine
drop-oldest** (the documented "drain one then re-send", or a drop-oldest ring) before
reuse, with a TDD property test under sustained overflow asserting the survivors are the
**newest** units (currently fails — write it first).

**Stream loss vs reference loss — distinct, both fail-honest:**
- **Stream loss** → per-input `AudioStore` silence-fill; tile rides LIVE→STALE→NO_SIGNAL; `SignalLoss` + `Silence` alarms (`alarm.rs:88/104`).
- **Reference (PTP) loss** → output **free-runs** on its monotonic `TimeSource` and is **never gated** by sender/receiver PTP state. Inv #1 keeps the local program running — this is **Multiview's own engineering choice**, not the rejection of a standardized failsafe (AES67/ST 2110-30 mandate *no* "mute on unsync"; typical endpoints *drop/invalidate* the stream on PTP loss, they don't deliberately mute). Add **`AlarmKind::ReferenceLoss`** (non-breaking, `#[non_exhaustive]`).
- **Receiver reference-loss algorithm (specified, not hand-waved).** While `Locked`/`Holdover`, place samples via `publish_at` using the last-disciplined media-clock origin. On `Freerun`, fall back to **wall-clock/local-rate arrival placement** (the existing IngestPump "stamp arrival as now" posture `transport.rs` already uses for video), mark the receiver's PTS **estimated**, and raise `ReferenceLoss` (cleared on reacquire). Test: linuxptp soft-GM + forced GM drop asserts output frame count unchanged across PTP loss, program not muted, `ReferenceLoss` raised/cleared, received-stream gap still silence-fills.

**PLC v1 = silence-substitution + a short fade ramp** at conceal-span edges. As-built
`AudioStore.read()` overlays live samples onto a zero buffer → a hard step discontinuity
(the documented click); no fade/ramp primitive exists in the audio crate. v1 adds the edge
ramp; stateful waveform/interpolation PLC is v2.

**ST 2022-7: receive IN scope, send OUT (v2).** The essence-agnostic `HitlessReconstructor`
+ `DualPathPacketSource` merge by RTP seq for any essence; AES67-RX reuses them behind
config. The audio depacketizer runs **downstream** of the merge. Plain Dante AES67 is
single-path ([ADR-T010](../decisions/ADR-T010.md)), so 2022-7 only engages with true dual-flow
ST 2110-30 senders. **Guard:** the reconstructor keys on seq with no SSRC check — two
mis-wired unrelated senders on path A/B could seq-collide and silently mis-merge. Before
engaging dual-path RX, validate the two flows **share SSRC** and satisfy the ST 2110-30
constraint (redundant streams must NOT share *both* identical source AND identical
destination addresses); reject otherwise. Add a dual-path audio merge property test for
L16/L24. Dual-NIC PTP-locked **egress** is deferred.

---

## 6. Discovery tiers

**Tier 0 (manual/static SDP)** — operator pastes an SDP or SDP-URL; zero discovery
protocol; works against any AES67 device; cheapest first win.

**Tier 1 (SAP, plug-and-play)** — RFC 2974, UDP **9875**. **Do not hardcode one group**;
announce/listen on a **set** of well-known defaults:
- **`239.255.255.255:9875`** — the **de-facto AES67/Dante SAP group** that shipping gear actually uses (Biamp Tesira, Shure, Sonifex, Q-SYS all list it). **Required** for the Dante-interop goal. The topic prompt's `239.255.255.255` is therefore **correct for interop** (it is *not* wrong); the receiver subscribes here by default.
- **`224.2.127.254:9875`** — RFC 2974's IETF-registered global-scope `sap.mcast.net`, for spec-pure / RAVENNA-via-RAV2SAP peers.

Receiver joins **both**; sender announces on the operator-selected group (default
`239.255.255.255` for plug-and-play Dante). Honour the **AES67 ≥30 s announce cadence**
(AES67-2018 tightens RFC 2974's 300 s `max()`-based interval down to ~30 s), the
10×-period/1 h purge, and `T`-bit deletion packets. Dante Controller does **not** route
AES67 — SAP/SDP is the only Dante discovery path (legacy Dante needs DDM to proxy
SDP→SAP). **Entirely unbuilt.** Document (see [ADR-0041](../decisions/ADR-0041.md) /
[sap-discovery](sap-discovery.md)) that `224.2.127.254` is the registered group while
`239.255.255.255` is what shipping AES67/Dante gear uses.

**Tier 2 (NMOS IS-04/IS-05)** — **already substantially built and tested** (served Node
API, staged/active + scheduled-TAI activation). Note: [ADR-T010](../decisions/ADR-T010.md)
was written framing discovery purely as SAP/SDP (it
never mentions NMOS) — this brief adds the NMOS tier. Remaining live work (DNS-SD over the
wire, multicast bind on IS-05 activation, the **audio** SDP `transport_file`) overlaps the
SAP/SDP work, so sequence **NMOS-finish *after* AES67 SDP/SAP**. **Do not block
plug-and-play on NMOS** — Dante interop depends on SAP/SDP, not NMOS.

### 6.1 SDP — one parser only (`multiview-input/src/st2110/sdp.rs`)

The existing `parse_sdp_transport` (`is05.rs:340`) handles **only** `c=`/`m=video`/
`a=source-filter` — no `m=audio`, no `a=rtpmap` L16/L24/rate/channels, no `a=ptime`/
`a=ts-refclk`/`a=mediaclk`, **no generator**. Build **one** AES67-audio SDP parse+generate
in `st2110/sdp.rs` (next to `v30`), with an `a=rtpmap` parser (`parse_rtpmap`,
PT/name/clock/channels) specialized to L16/L24 audio. It fills the depacketizer's
channels/depth(L16|L24) + ptime + ts-refclk (GMID/domain) + mediaclk offset; the
control-plane IS-05 `transport_file` path **delegates** to it (one audio-SDP parser, DRY).

Generate (Class A, L24 stereo example):
```
m=audio <port> RTP/AVP 98
a=rtpmap:98 L24/48000/<ch>
a=ptime:1
a=ts-refclk:ptp=IEEE1588-2008:<GMID>:<domain>     # also handle the localmac form
a=mediaclk:direct=0
```

---

## 7. Crate / feature / config surface

**No new crate.** Extend existing seams: ingest in `multiview-input/src/st2110/`
(`audio_producer` + `sdp` + `sap`), egress in `multiview-output`, format/capability in
`multiview-audio`, PTP in `multiview-engine`. **No `multiview-ptp-sys`** — servo math is
pure + tested; the PHC binding is the `ptp` feature (safe `rustix`); linuxptp runs as a
separate OS process (no linking, no licence obligation).

**Features.** Reuse the off-by-default **`st2110`** feature (`= ["tokio/net"]`,
`multiview-input/Cargo.toml:97` — no FFI, `unsafe_code = forbid`) for ingest; add a
**symmetric egress feature** in `multiview-output`. The boundary resampler pulls `rubato`
(pure-Rust, MIT/Apache, LGPL-clean), **never** the `ffmpeg`-gated resampler. User-facing
term is "AES67"; code/feature aligns to `st2110`.

**Config** (`multiview-config/src/schema.rs`). Add an **internally-tagged**
(`#[serde(tag="kind", rename_all="snake_case")]`, `#[non_exhaustive]` — **never
untagged**) variant to **both**:
- `SourceKind` (`schema.rs:213`) — `Aes67` (alias `st2110_audio`). Binds by static SDP/SDP-URL (Tier 0), SAP session id (Tier 1), or NMOS sender id (Tier 2); carries multicast group/port + channel/depth(L16|L24)/ptime hints + optional `link_offset_ms` + PTP domain. Modelled like the NDI name-bound variant; first-class peer via the existing `Source` `#[serde(flatten)] kind`.
- `Output` (`schema.rs:531`) — AES67-out is the **first output with no encode/gpu_pin stage** (raw PCM packetization). (The asymmetry is *not* "first non-codec output" — `Output::Ndi` already omits `codec`; the novelty is omitting **gpu_pin/encode entirely**.) Reuse the `OutputAudio` program-bus selector. **Handle the exhaustive const-fn matches** `Output::gpu_pin()` (`schema.rs:633`) and `Output::audio()` (`schema.rs:647`): either carry an always-`None` `gpu_pin` so the matches stay mechanical, or add an explicit `Aes67 => None` arm — with a compile-test that all `Output` variants are reachable by both fns.

**Control plane.** Expose the AES67 Sender + Receivers as IS-04 resources
(`MediaFormat::Audio`, transport `urn:x-nmos:transport:rtp.mcast`). A read-only `/api/v1`
discovery endpoint lists SAP + NMOS sessions. Binding a receiver group to a tile is
**Class-1** live-apply (inv #11 — it only changes what is sampled).

---

## 8. Test strategy (no proprietary hardware)

- **In-memory codec round-trip** (no NIC): `Aes67Packetizer` encode → `V30Payload::parse` decode, full-scale L24 monotonic + never sign-flips.
- **Send/receive RTP loopback** on `lo` / a local multicast group (gated `#[ignore]`/feature): the AES67 wire-contract proof — *not* Dante-branded behaviour.
- **PTP** via linuxptp `ptp4l`/`phc2sys` as a software grandmaster (media-profile sync intervals + DSCP/EF) + forced GM drop for the reference-loss test.
- **Drift soak** (resampler): inject a deliberate ppm offset, 72 h soak per [ADR-T006](../decisions/ADR-T006.md), assert zero gaps + zero audible drop/dup.
- **Chaos gate (inv #10):** wedge the send socket; output clock keeps ticking, drops counted.
- **Honesty caveat ([ADR-T010](../decisions/ADR-T010.md)):** Dante Virtual Soundcard is **not** AES67-capable — it cannot be the interop target; real Dante-over-AES67 needs AES67-capable hardware + SAP.

---

## 9. Citations

- AES67 / clocking / RTP timestamp / mediaclk offset (Audinate, AES67, RAVENNA/AIMS): AES67 summary <https://en.wikipedia.org/wiki/AES67>; Audinate AES67 config / creating AES67–ST 2110-30 flows <https://dev.audinate.com/GA/dante-controller/userguide/webhelp/content/aes67_config.htm>; Audinate clock synchronization (PTPv1/v2, leader election) <https://dev.audinate.com/GA/dante-controller/userguide/webhelp/content/clock_synchronization.htm>; DDM AES67 vs SMPTE domains <https://dev.audinate.com/GA/ddm/userguide/1.1/webhelp/content/appendix/aes67_and_smpte_domains.htm>; DVS not AES67-capable <https://www.mdw.ac.at/aesr-lab/docs/A-System/Networked-audio/Dante-AES67-interoperability/>.
- RFC 3190 (L16/L24 over RTP), RFC 3550 (RTP), RFC 3551 §4.1/§4.3 (audio RTP clock independent of channel count; timestamp = first-sample instant) + §4.5.11 (L16 sample format), RFC 2974 (SAP), RFC 4566/8866 (SDP), RFC 7273 (`ts-refclk`/`mediaclk`).
- SMPTE ST 2110-30 (Class A/B/C, AX/BX/CX), ST 2110-10 §7.5 (1970 TAI epoch media clock), SMPTE ST 2059-1/-2 (PTP profile), IEEE 1588-2008.
- SAP group de-facto `239.255.255.255` — Biamp Tesira, Shure, Sonifex, Q-SYS AES67 docs.
