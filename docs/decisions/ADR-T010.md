# ADR-T010 — Audio-over-IP: AES67 / ST 2110-30 (open) is the Dante interop path

**Status:** Accepted (2026-06-06; revised 2026-06-09 — native Dante is **not** a supported
feature). Supersedes the earlier framing that kept native Dante as an optional licence-gated
binding.
**Area:** streaming/timing (T) — audio-over-IP transport
**Drivers:** operator requirement for audio-over-IP that reaches Dante facilities; the
open-standards-only + LGPL-clean-default mandate (CLAUDE.md,
[conventions §7](../architecture/conventions.md)).
**Related:** [ADR-0033](ADR-0033.md) (the AES67/ST 2110-30 send+receive *implementation* under
this decision), [ADR-0008](ADR-0008.md) (NDI runtime-load/licence gate — a *different* posture,
not the model here), [ADR-T003](ADR-T003.md) (unified timing), the ST 2110 + PTP work (IN-1/IN-2),
[ADR-R005](ADR-R005.md) (audio mix/route + capability matrix).

## Context

Operators need Multiview to send and receive audio over IP and to interoperate with Dante
(Audinate) facilities, the dominant pro-AV audio-over-IP system. There are two ways to reach a
Dante network:

1. **Native Dante** — a closed, proprietary protocol (non-RTP UDP framing, the closed ConMon
   subscription/control protocol, PTPv1 clocking by default). Audinate publishes no on-wire spec;
   every implementable native path requires a licensed Audinate SDK/IP (DAL / Dante Embedded
   Platform / Brooklyn/Ultimo HW) under OEM/NDA + royalties.
2. **AES67 / SMPTE ST 2110-30** — the open, royalty-free interop standard. It is pure
   RFC/AES/SMPTE (RTP + L16/L24 PCM + SDP + SAP + PTPv2), implementable with **zero Audinate IP**,
   and it is **Audinate's own documented licence-free bridge** to Dante networks. Multiview
   **already owns the load-bearing part**: the ST 2110-30 / AES67 L16/L24 PCM depacketizer + RTP
   parser (`multiview-input/src/st2110/v30.rs`, property-tested) and a PTP servo.

We originally hoped to support native Dante as well, as an off-by-default, never-vendored,
operator-licence-attested feature mirroring the NDI posture ([ADR-0008](ADR-0008.md)). We are not
pursuing that. We asked Audinate directly; their response (on file) was that their SDKs are
"intended for products operating under commercial licensing agreements" and that they are therefore
"not able to provide guidance or support for integrating Dante functionality into open-source
projects." With no licence-free path to a native implementation — and a reverse-engineered one not
being shippable for an open product — native Dante is not buildable for Multiview. (It is also our
view, separately, that gating a protocol this central to professional audio behind a commercial
arrangement raises the barrier for newcomers; that is opinion, stated as such.) The open AES67 /
ST 2110-30 standards are free for anyone to use and still interoperate with Dante through Dante's
AES67 mode, so choosing open standards loses no real Dante reach. (This revises the earlier
acceptance of this ADR, which had kept native Dante as an optional gated binding.)

## AES67 / ST 2110-30 is the open interop path Audinate itself documents

- Dante's **AES67 mode** transmits/receives AES67 multicast RTP to/from non-Dante
  devices; **ST 2110-30 mode** (firmware v4.2+) adds SAP/SDP advertisement + configurable
  PTPv2 domains. This is the *only* license-free way to exchange audio with Dante. *(Confirmed.)*
- **Dante-AES67 interop budget (the format clamp Multiview must hit):** **L24, 48 kHz**
  (96 kHz on supporting gear), **1 ms packet time** (48 samples/ch/pkt), **≤8 channels per
  AES67 flow**, **multicast in 239.x/16**, **PTPv2 fixed domain 0**, discovery via **SAP/SDP**.
  (ST 2110-30 mode unlocks 125 µs profiles + up to 64 ch.) *(Confirmed.)*
- **Limitations to design around:** Dante↔Dante always uses native transport (AES67 is
  non-Dante-only); per-device enablement; multicast-only/non-redundant; Dante Controller
  does **not** route AES67 flows — discovery/subscription is via **SAP/SDP** (older Dante
  needs DDM to proxy SDP→SAP). *(Confirmed.)*

## Decision

1. **Implement AES67 / SMPTE ST 2110-30 audio I/O as the Dante interop path** from the open
   specs, with **zero Audinate IP**, reusing the existing ST 2110-30 depacketizer + PTP servo.
   Dante devices interoperate through their AES67 mode. Interop budget: **L24, 48 kHz, 1 ms ptime,
   ≤8 ch/flow, multicast 239.x/16, PTPv2 domain 0, SAP/SDP discovery**. This is `AES67-1..5` in
   the plan; the send/receive implementation is specified in [ADR-0033](ADR-0033.md).
2. **Native Dante is NOT supported** (per the licensing/complexity analysis above): it requires a
   licensed Audinate SDK which, per Audinate's response to us (on file), is intended for products
   operating under commercial licensing agreements and is not available for open-source
   integration, and a reverse-engineered native implementation is not shippable for an open
   product. There is no `multiview-dante-sys` leaf, no `DanteLicense` gate, and no native-Dante
   feature in the build.
3. **Timing:** be a PTPv2 follower on an AES67 / SMPTE ST 2059 grandmaster and use it as the
   **media-clock reference**, never as a pacer — the output tick stays the sole pacer
   (invariant #1; ADR-T003). The AoIP send/recv runs off the engine hot loop with bounded
   drop-oldest buffers (invariant #10).

## Consequences

- Dante audio in/out via open AES67 / ST 2110-30 **without licensing Audinate**, on the standard
  the rest of the IP-video plant already speaks (ST 2110-30 / ST 2059), reusing tested code.
- New open work, none requiring proprietary IP: an audio `FrameProducer` over `v30`, an
  AES67-audio SDP parse/generate, a SAP announce/listen layer, an AES67 / ST 2110-30 **RTP
  transmit** path, and PTP profile/domain config.
- Limits from the AES67 standard (per-device enablement, multicast-only, non-redundant for plain
  Dante, ≤8 ch/flow, no Dante-Controller routing → SAP/SDP discovery, older gear needs DDM to
  proxy SDP→SAP) are documented and surfaced honestly, not hidden.
- **Test honesty:** Dante Virtual Soundcard is **not** AES67-capable, so it cannot be the
  interop target. CI proves the AES67 wire contract via a gated loopback; real
  Dante-over-AES67 needs AES67-capable hardware + SAP.
- Default build stays LGPL-clean and proprietary-free.

## Alternatives rejected

- **Native Dante in the default build** — impossible without licensed Audinate IP;
  reverse-engineered implementations are not shippable for an open product. Rejected.
- **Native Dante as an optional, licence-gated binding** (the earlier position in this ADR) —
  it would still depend on an Audinate SDK which, per Audinate's response to us (on file), is
  intended for products under commercial licensing agreements and is not available for open-source
  integration, so it would not be reachable by the community in any case. AES67 already covers
  Dante interop on open standards. Rejected; native Dante is not a planned feature.
