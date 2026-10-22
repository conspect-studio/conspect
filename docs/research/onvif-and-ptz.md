# Multiview — ONVIF & PTZ — Discovery, Client Control, Outputs-as-ONVIF, and the PTZ Control Standard

**Area:** Management (Devices domain) / Control plane / Output facet.
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0062](../decisions/ADR-0062.md) (ONVIF client + WS-Discovery + manual cross-subnet endpoint add) · [ADR-0063](../decisions/ADR-0063.md) (ONVIF server — present Multiview outputs as ONVIF/RTSP devices; PTZ passthrough) · [ADR-0064](../decisions/ADR-0064.md) (PTZ control model — ONVIF PTZ + VISCA / VISCA-over-IP profile).
**Extends:** [managed-devices.md](managed-devices.md) (the Devices domain, the compiled-in driver model, mDNS discovery, confirm-adopt), [ADR-M008](../decisions/ADR-M008.md) (registry + closed driver enum), [ADR-M009](../decisions/ADR-M009.md) (source/output/display projections), [ADR-0041](../decisions/ADR-0041.md) (untrusted-inventory confirm-adopt doctrine), [ADR-0006](../decisions/ADR-0006.md) (RTSP serving), [ipv6-first.md](ipv6-first.md) ([ADR-0042](../decisions/ADR-0042.md)), [decoupled-routing.md](decoupled-routing.md) (per-stream crosspoints — the relay path PTZ passthrough rides on).
**Relates to:** [unifi-protect-compat.md](unifi-protect-compat.md) (a sibling vendor-camera-ecosystem brief in this batch), [output-metadata-and-orientation.md](output-metadata-and-orientation.md) (output metadata the ONVIF-server facet advertises), [url-input.md](url-input.md).
**Backlog:** `ONVIF-*` / `PTZ-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> **The operator asked for ONVIF — all of it, not a bare-bones subset.** Discover ONVIF
> devices on the LAN; add an ONVIF endpoint manually across a subnet boundary (test, then
> adopt); control the camera through ONVIF the way a real NVR does (set its time, read and set
> codecs/resolution/profiles, drive its PTZ); let our *own* outputs be discovered and consumed
> by third-party ONVIF/RTSP clients where the transport and codec actually permit it; and where
> an output is a passthrough/transcode of an upstream ONVIF/VISCA camera *through a layer*, relay
> PTZ down to that camera. And: **is there a PTZ standard?** Yes — two, and we speak both:
> **ONVIF PTZ** and **VISCA / VISCA-over-IP**. This brief specifies all of that as a new
> **facet of the already-built Devices domain**, with zero new engine surface and both isolation
> invariants safe by construction.

---

## 0. Headlines

1. **Two directions, one domain.** ONVIF has a **client** side (Multiview *controls* cameras — the
   NVR role) and a **server** side (Multiview *is* an ONVIF device — the camera/encoder role). Both
   land as control-plane work inside the **already-built Devices domain** ([managed-devices.md](managed-devices.md),
   [ADR-M008](../decisions/ADR-M008.md)): a new `onvif` driver variant for the client, an opt-in
   ONVIF-server facet bolted onto existing outputs for the server. The engine sees only ordinary
   Sources and Outputs (`crates/multiview-config/src/schema.rs:217` `SourceKind`, `:683` `Output`),
   so invariant #10 holds the same way it does for the shipped `zowietek`/`cast` drivers.

2. **ONVIF is an open standard — we build a clean-room client and a clean-room server, bundling no
   vendor SDK.** ONVIF Core (WS-Discovery), Device, Media, Media2, Imaging, and PTZ are
   SOAP/WSDL web services published by the ONVIF organisation; the wire is ordinary
   SOAP-over-HTTP(S) plus WS-Discovery's SOAP-over-UDP-multicast. We implement the SOAP
   request/response shapes from the published specs (mirroring the `zowietek` clean-room posture in
   [managed-devices.md §12](managed-devices.md)) — no gSOAP, no vendor toolkit, `unsafe_code = forbid`.

3. **"Full ONVIF" is delivered by *modelling the camera as a managed device*, not by re-exporting
   the camera's whole feature tree.** The operator's "everything — time, codecs, res, etc." maps
   onto the existing three projections ([ADR-M009](../decisions/ADR-M009.md)) **plus a fourth, new
   PTZ facet**: the camera's streams become **source candidates**; its Device-service knobs (time,
   network, users) and Media-service knobs (encoder config: codec/resolution/bitrate/GOP/FPS) become
   **management actions**; its Imaging knobs (focus/iris/brightness) become management actions; its
   PTZ service becomes a **first-class PTZ facet** (§5). We expose a deep-but-bounded surface, never
   an open property bag (the typing/YAGNI guard of [ADR-M008](../decisions/ADR-M008.md) holds).

4. **Discovery reuses the shipped mDNS/confirm-adopt machinery — and adds the ONVIF-native path.**
   ONVIF devices announce via **WS-Discovery** (SOAP-over-UDP to `239.255.255.250:3702`, IPv6
   `ff02::c`, type `NetworkVideoTransmitter`) — *not* mDNS. The Devices domain already has a
   bounded, single-flight, untrusted-inventory discovery layer
   (`crates/multiview-control/src/devices/discovery.rs`) feeding a confirm-adopt flow
   ([ADR-0041](../decisions/ADR-0041.md)); WS-Discovery is a **new browse transport** plugged into
   that same untrusted-inventory pipeline (a sibling to the mDNS browse in
   `crates/multiview-mesh/src/service.rs:37`), with a `DiscoveryDriverKind::Onvif` added to the
   closed kind enum (`crates/multiview-control/src/devices/discovery.rs:113`).

5. **Manual cross-subnet add is a first-class, must-have path — because WS-Discovery is link-local.**
   Multicast WS-Discovery does not cross a router by default, so the operator must be able to type an
   ONVIF endpoint (`https://[2001:db8::10]/onvif/device_service`), **test** it (an unauthenticated
   `GetSystemDateAndTime` then an authenticated `GetDeviceInformation`/`GetCapabilities` probe), and
   **adopt** it — exactly the confirm-adopt doctrine, just seeded by a typed endpoint instead of a
   discovery hit.

6. **Outputs-as-ONVIF only where a generic ONVIF/RTSP client can actually consume the
   transport+codec.** The ONVIF-server facet advertises a Multiview **`RtspServer` output**
   (`crates/multiview-config/src/schema.rs:685`) carrying **H.264 or H.265 over RTSP** — H.264/RTSP
   is the universal ONVIF Profile-S baseline; H.265 is Profile-T (the advertised scopes/profile
   claims are gated by codec, §4.1). HLS/LL-HLS/RTMP/SRT/NDI outputs are **not** ONVIF-consumable
   and are not advertised. The facet is **opt-in per output** and rejects an output whose codec or
   transport an ONVIF client cannot play (a typed capability error, never a silent half-feature).

7. **PTZ passthrough is a relay, not a re-implementation.** When an output is a passthrough/transcode
   of a *single upstream ONVIF (or VISCA) camera through a layer* (a full-frame tile, or a 1:1
   transcode), the ONVIF-server facet can expose a PTZ service whose moves are **relayed to that
   upstream camera's driver** via the existing per-stream routing model
   ([decoupled-routing.md](decoupled-routing.md)). When the output is a true multi-tile composite,
   PTZ is **not offered** (there is no single physical camera to steer) — honest by construction.

8. **Yes, there is a PTZ standard — two, and we speak both.** **ONVIF PTZ** (AbsoluteMove /
   RelativeMove / ContinuousMove / Stop, presets, PTZ nodes + coordinate **spaces**) is the IP-CCTV
   standard; **VISCA** (and **VISCA-over-IP**, commonly UDP/TCP `52381`) is the broadcast/PTZ-camera
   standard. [ADR-0064](../decisions/ADR-0064.md) pins a **vendor-neutral internal PTZ command model**
   with two driver profiles (ONVIF-PTZ, VISCA-IP) underneath it, so a tile bound to an ONVIF camera
   and a tile bound to a VISCA camera present the operator the same controls.

9. **Both isolation invariants are safe with zero engine change.** Every ONVIF/PTZ poller and SOAP
   call is a **control-plane actor** publishing into watch/broadcast channels (the
   `crates/multiview-control/src/devices/` actor pattern); none touches the output-clock loop, none
   is awaited by the engine. A camera that is slow, hung, or hostile can stall **only its own driver
   task**. Program output is never contingent on any device (invariant #1).

---

## 1. Scope — client vs server vs PTZ, and what is explicitly out

Three deliverables, each its own ADR, all inside the Devices domain.

| # | Deliverable | Role | ADR | Crate seam |
|---|---|---|---|---|
| **C** | **ONVIF client** — Multiview controls cameras (the NVR role): discover, manually add cross-subnet, set time, read/set codecs/res/profiles, drive Imaging, drive PTZ | We are the *operator* of someone else's camera | [ADR-0062](../decisions/ADR-0062.md) | new `onvif` driver under `crates/multiview-control/src/devices/` |
| **S** | **ONVIF server** — Multiview *is* an ONVIF device: a qualifying `RtspServer` output is discoverable + controllable by a third-party NVR/VMS | We are the *camera/encoder* | [ADR-0063](../decisions/ADR-0063.md) | new server facet on `multiview-control`, advertising existing outputs |
| **P** | **PTZ model** — one internal PTZ command model, two driver profiles (ONVIF-PTZ, VISCA / VISCA-over-IP), plus the server-side PTZ-passthrough relay | The shared control vocabulary both directions use | [ADR-0064](../decisions/ADR-0064.md) | `ptz` submodule shared by the client driver and the server facet |

**In scope.** ONVIF Core/WS-Discovery, Device, Media + Media2, Imaging, PTZ; SetSystemDateAndTime;
GetProfiles/Get·SetVideoEncoderConfiguration; the full PTZ verb set + presets + nodes/spaces; VISCA +
VISCA-over-IP as the alternate PTZ profile; outputs-as-ONVIF for RTSP H.264/H.265; PTZ passthrough for
single-camera layer/transcode outputs; manual cross-subnet endpoint test+add.

**Out of scope (named, not silently dropped).**
- **ONVIF Profile-S/T server *certification*.** We aim for *interoperability* with generic ONVIF
  clients, not a logo. Certification is a separate, paid, hardware-gated program; we ship the wire
  behaviour and document the conformance gaps honestly (an open question, §8).
- **ONVIF analytics / events / recording / replay / access-control services** (Profile-A/C/D/G beyond
  what Media2 needs). The client reads what it needs to drive a managed camera; we do not become an
  ONVIF analytics consumer. (Object detection has its own brief, [object-detection-ai.md](object-detection-ai.md).)
- **ONVIF G recording-search on the server side** — we are not an ONVIF NVR-as-a-service.
- **FreeD / position-telemetry output** is noted as a *related* PTZ-adjacent standard (§5.5) but is a
  one-line forward-reference, not built here.
- **WS-Discovery as a media-bind shortcut** — discovery yields *candidates requiring confirm-adopt*,
  never an auto-ingested stream ([ADR-0041](../decisions/ADR-0041.md)).

---

## 2. Discovery — WS-Discovery + manual cross-subnet add

### 2.1 The ONVIF-native discovery transport (new)

ONVIF devices implement **WS-Discovery** (OASIS WS-Discovery 1.1): a Target Service multicasts a
`Hello` on join and answers a `Probe` with a `ProbeMatch`. (verified: ONVIF Application Programmer's
Guide; OASIS WS-Discovery.) Concretely:

- **Multicast group + port:** IPv4 `239.255.255.250:3702`, IPv6 **`ff02::c` port 3702**
  (link-local scope). (verified: WS-Discovery spec / Wikipedia summary; ONVIF uses the standard
  WS-Discovery group.) **IPv6-first** per [ADR-0042](../decisions/ADR-0042.md): we send the IPv6
  Probe first and join `ff02::c`; IPv4 `239.255.255.250` is the legacy peer.
- **Probe type:** `dn:NetworkVideoTransmitter` (an IP camera/encoder), scoped by ONVIF profile scope
  URIs (`onvif://www.onvif.org/Profile/Streaming`, …). (verified: ONVIF APG / hacktricks
  WS-Discovery notes — `NetworkVideoTransmitter` is the camera type.)
- **Payload:** SOAP 1.2 over UDP — the same SOAP model the client uses for the unicast services,
  so the SOAP envelope codec is built **once** and shared (DRY, mirroring the
  single-SAP/single-SDP discipline of [ADR-0041](../decisions/ADR-0041.md) for that protocol).

This is **not mDNS** — so it is a *new* browse transport, a sibling to the shipped mDNS daemon
(`crates/multiview-mesh/src/service.rs:37` wraps `mdns_sd` for RFC 6762/6763). It plugs into the
**existing** untrusted-inventory discovery pipeline:

- a `DiscoveryDriverKind::Onvif` variant added to the closed kind enum
  (`crates/multiview-control/src/devices/discovery.rs:113` — today `Cast`/`ZowietekControl`/`Ndi`/`Unknown`);
- the same **bounding** the shipped layer already enforces — single-flight, time-budgeted, a hard cap
  on collected services, every responder-controlled string truncated before retention
  (`crates/multiview-control/src/devices/discovery.rs` header doc, §"Hostile-responder bound"). A
  WS-Discovery responder is unauthenticated and spoofable, so the same hardening that protects the
  mDNS path protects this one;
- results surfaced as **`device.discovered`** rows on the realtime `Topic::Devices`
  ([ADR-RT007](../decisions/ADR-RT007.md)) while a scan operation runs — **never** auto-adopted.

### 2.2 Manual cross-subnet endpoint test + add (must-have)

WS-Discovery multicast is **link-local** — it does not cross a router. The operator's explicit ask
("manual ONVIF endpoint (cross-subnet) test + add") is therefore a **first-class** path, not a
fallback. The flow, riding the existing confirm-adopt doctrine:

1. **Operator types a device service URL** — IPv6-first, bracketed:
   `https://[2001:db8::10]/onvif/device_service` (HTTP allowed on a management VLAN with a warning,
   matching the `zowietek` plain-HTTP posture in [managed-devices.md §3.1](managed-devices.md)).
2. **Test (unauthenticated reachability):** `GetSystemDateAndTime` is the canonical *no-auth* ONVIF
   probe (it must answer before WS-Security, so a client can correct its clock for the WS-UsernameToken
   nonce/created timestamp). A 200 with a parseable `SystemDateAndTime` proves "this is an ONVIF
   device and here is its clock skew".
3. **Test (authenticated capability):** with credentials, `GetDeviceInformation` +
   `GetCapabilities` (or `GetServices`) returns model/firmware and the **service endpoint map** (Media,
   Imaging, PTZ URLs may live on different paths/hosts). We record those endpoints.
4. **Adopt:** the operator confirm-binds, creating a normal `Device` record
   (`crates/multiview-config/src/device.rs`) with `driver = "onvif"` — the same `POST`/confirm
   surface the device routes already expose (`crates/multiview-control/src/routes/mod.rs:672`).

The "test" verbs are **read-only** and bounded; "add" is the existing idempotent adopt. A bad URL,
wrong creds, or a non-ONVIF endpoint each produces a typed problem (`application/problem+json`, RFC
9457) that the SPA shows on the field — distinguishing **unreachable** vs **not-ONVIF** vs
**auth-failed** (the `AUTH_FAILED` lifecycle state already exists,
`crates/multiview-control/src/devices/state_machine.rs:14`).

---

## 3. ONVIF client services (Device / Media / Media2 / Imaging / PTZ)

The `onvif` driver is a **control-plane poller/actor** exactly like `zowietek`
(`crates/multiview-control/src/devices/zowietek/`): it owns a typed SOAP client, serialises requests
per device, polls status at ≤1 Hz, rides the `ADOPTING → ONLINE / DEGRADED / AUTH_FAILED /
UNREACHABLE` lifecycle (`state_machine.rs`), and maps the camera into projections + management actions.

### 3.1 Authentication

ONVIF security is **WS-Security UsernameToken** (digest: `Base64(SHA1(nonce + created + password))`)
and/or HTTP Digest, over HTTP or HTTPS. (verified: ONVIF Core / Device-service security model — the
clock-correction-before-auth requirement is why `GetSystemDateAndTime` is unauthenticated.) The driver:
- corrects local↔device clock skew from the `GetSystemDateAndTime` probe before stamping the
  `Created` element (a common real-world failure: a device whose clock is wrong rejects every
  authenticated call);
- stores credentials as a write-only `secret_ref` reusing the existing secret model
  (`crates/multiview-config/src/schema.rs` secret refs; the device record's `[devices.auth]` block,
  [managed-devices.md §7.3](managed-devices.md));
- treats plain-HTTP as a **management-VLAN** recommendation, never the default.

### 3.2 Device service — "set the time, the network, the users"

| Operation | ONVIF op (verified family) | Multiview surface |
|---|---|---|
| Read clock / correct skew | `GetSystemDateAndTime` | probe + status |
| **Set the camera's time** | **`SetSystemDateAndTime`** (required for Profile-S conformance; probed, not assumed — web-verified: ONVIF Profile-S spec) | management action; offer **"sync camera clock to Multiview / to NTP"** where the device supports it |
| Identity / firmware | `GetDeviceInformation` | status (model/firmware chip) |
| Capability/endpoint map | `GetCapabilities` / `GetServices` | drives which facets light up |
| Network config | `GetNetworkInterfaces` / NTP / hostname | management action (IPv6-first; bounded) |
| Users | `GetUsers` (create/delete behind an explicit operator gate) | management action, opt-in |
| Reboot / factory-default | `SystemReboot` | `POST …/reboot` (the route exists, `routes/mod.rs:682`) |

Setting the camera's clock is load-bearing for the product: a camera whose PTS timeline is sane is a
better-behaved Source. `SetSystemDateAndTime` is **required for Profile-S-conformant** devices, but
fielded cameras are often partial/nonconformant — so the driver **probes capability and degrades
gracefully** (offers the action only where the device advertises/accepts it) rather than assuming it
is universally present.

### 3.3 Media / Media2 service — "codecs, resolution, profiles"

ONVIF **media profiles** bind a video-source + video-encoder + (PTZ/audio/metadata) configuration; an
RTSP stream URI is fetched per profile. (verified: ONVIF Media2 `GetProfiles` returns the configured
profiles; each profile is the unit of stream config.) The driver:

- **Reads** `GetProfiles` → for each profile, the **video encoder config** (codec H.264/H.265,
  resolution, bitrate, GOP, frame-rate) and `GetStreamUri` (the RTSP URL we ingest);
- maps each profile to a **source candidate** ([ADR-M009](../decisions/ADR-M009.md)): picking one
  creates an ordinary `SourceKind::Rtsp` Source (`schema.rs:296`) carrying a `device_ref` — the
  ingest path (pacer, framestore ladder, supervised reconnect) is untouched, exactly as for the
  `zowietek` RTSP candidates;
- **Writes** `SetVideoEncoderConfiguration` (both Media1 `ver10` and Media2 `ver20` name the write
  op in the singular — Media2 differs by namespace/binding, not op name) to
  drive the operator's ask of **setting codec/resolution/bitrate** on the camera — honestly
  classified per invariant #11: changing the camera's encoder is a **device-side reset** (its stream
  restarts; a bound Source rides the tile ladder to NO_SIGNAL and back), the `DEV` capability-matrix
  class [managed-devices.md §10](managed-devices.md) introduced. We **decode at display resolution**
  (invariant #6): where the camera offers a sub-stream profile near a tile's pixel size, the source
  candidate prefers it.

**Media vs Media2:** Media2 (`ver20`) is the current service; Media1 (`ver10`) is the legacy one many
fielded cameras still speak. The driver negotiates from `GetServices`/`GetCapabilities` and uses Media2
when present, Media1 otherwise — one internal encoder-config model, two SOAP bindings (the
profile-negotiation pattern, not two code paths in the projection layer).

### 3.4 Imaging service

`Get·SetImagingSettings` (brightness/contrast/white-balance/exposure/focus) and `Move`/`Stop` for
**focus** map to bounded management actions. Focus is *imaging*, not PTZ in ONVIF; we surface it
alongside PTZ in the UI but route it through the Imaging service.

### 3.5 PTZ service — see §5

PTZ is large enough to be its own section and its own ADR ([ADR-0064](../decisions/ADR-0064.md)).

---

## 4. ONVIF server — Multiview outputs as ONVIF devices

The operator's ask: *"outputs with compatible transports/codecs can be enabled for ONVIF
discovery."* We make a qualifying Multiview output **discoverable and controllable as an ONVIF
`NetworkVideoTransmitter`** so a third-party NVR/VMS can find and pull it like any camera.

### 4.1 Which outputs qualify (the hard gate)

**Only an output a generic ONVIF/RTSP client can actually consume.** Concretely:

| Output kind (`schema.rs:683`) | ONVIF-consumable? | Why |
|---|---|---|
| `RtspServer` (`:685`) **H.264 or H.265** | **Yes** | ONVIF Profile-S streaming is RTSP/RTP **H.264**; **H.265** is Profile-T. The advertised profile scope is gated by codec — H.264 outputs advertise a Profile-S-compatible scope, H.265 outputs a Profile-T-compatible scope — and validated against clients accordingly (an NVR that only speaks Profile S will not be offered an H.265 profile) |
| `LlHls` / `Hls` (`:708`/`:734`) | No | ONVIF clients pull RTSP, not HLS |
| `Rtmp` (`:770`) / `Srt` (`:798`) | No | Not an ONVIF transport |
| `Ndi` (`:754`) | No | NDI has its own discovery (`_ndi._tcp`); not ONVIF |

The facet is **opt-in per output** (a new `onvif_server: Option<…>` block on the `RtspServer` output,
additive + `#[serde]`-defaulted, the established schema-evolution pattern). Enabling it on a
non-qualifying output is a **typed validation error** at config admission — never a silent no-op. This
is the same honesty discipline as the recording validator rejecting `ByteExact` on non-byte transports
([ADR-0037](../decisions/ADR-0037.md) §6).

### 4.2 What the server facet implements

A minimal but **real** ONVIF service set, enough for a generic NVR to discover, authenticate, list a
profile, fetch the RTSP URI, and (where applicable, §4.3) drive PTZ:

- **WS-Discovery responder** — answer `Probe` for `NetworkVideoTransmitter` on `ff02::c` / `239.255.255.250:3702`, advertising the streaming-profile scope. (This is the announce half of §2's transport, reusing the same SOAP-UDP codec.)
- **Device service** — `GetDeviceInformation`, `GetSystemDateAndTime` (read-only),
  `GetCapabilities`/`GetServices`, `GetNetworkInterfaces`. We advertise *our* identity (Multiview,
  version), not a camera's. **`SetSystemDateAndTime` is *not* honoured on the server facet** — letting
  a third-party NVR set the Multiview host clock would affect auth (WS-Security nonce/created), logs,
  scheduling, and the output timeline, so an incoming set is rejected (or a no-op acknowledgement);
  host time is managed only via the deliberate, privileged control-plane time action with declared
  impact, never via the ONVIF server surface.
- **Media2 service** — `GetProfiles` exposing **one profile per advertised output**, with the
  output's real codec/resolution/bitrate as a (mostly read-only) video-encoder config, and
  `GetStreamUri` returning the output's RTSP URL (`rtsp://[…]:8554<mount>`, the `RtspServer.mount`,
  `schema.rs` `mount`). The RTSP serving itself is the **existing** path
  ([ADR-0006](../decisions/ADR-0006.md)) — the ONVIF facet is *metadata + control on top of an
  already-served stream*, encode-once-mux-many preserved (invariant #7): a third-party ONVIF client is
  just another consumer of the existing rendition.
- **PTZ service** — only for passthrough outputs (§4.3); absent otherwise.
- **Imaging service** — generally **absent** (we have no physical imager); offered only as a thin
  passthrough where §4.3's single-camera relay applies and the upstream exposes Imaging.

Security: WS-Security UsernameToken / HTTP Digest on the server side too, backed by the existing
control-plane auth/secret model (`crates/multiview-control/src/auth.rs`, `jwt.rs`). The ONVIF-server
facet is **off by default** and, like all device-net I/O, behind an off-by-default feature
([ADR-M008](../decisions/ADR-M008.md) feature-gating posture).

### 4.3 PTZ passthrough — the operator's "PTZ through a layer" ask

> *"Where an output is a passthrough/transcode (through a layer, not program/preview) allow some
> ONVIF passthrough (e.g. PTZ)."*

When the advertised output is a **passthrough/transcode of a single upstream camera through a
layer** — i.e. the output's video is, at the relevant layer, sourced 1:1 from one ONVIF or VISCA
camera (a full-frame single-tile layout, or a per-camera transcode rendition) — the ONVIF-server PTZ
service is **real but a relay**:

- a third-party NVR issues `ContinuousMove`/`AbsoluteMove`/`GotoPreset` to *our* PTZ service;
- we resolve the **single upstream camera** behind that output via the per-stream routing model
  ([decoupled-routing.md](decoupled-routing.md) — the output's video crosspoint resolves to one
  input, that input is a `device_ref` ONVIF/VISCA camera);
- we **relay** the move through that camera's driver (ONVIF-PTZ or VISCA, §5) using the shared
  internal PTZ command model ([ADR-0064](../decisions/ADR-0064.md)).

**When PTZ passthrough is offered vs not:**

| Output composition | PTZ service on the ONVIF facet | Why |
|---|---|---|
| Full-frame single tile bound to one PTZ camera | **Yes — relayed** | exactly one physical camera to steer |
| Per-camera transcode rendition of one PTZ camera | **Yes — relayed** | same |
| Multi-tile composite / program / preview | **No** | no single camera; steering would be ambiguous/meaningless |
| Single tile bound to a non-PTZ source (file, NDI, fixed cam) | **No** (PTZ node absent) | nothing to steer |

This is honest by construction: the PTZ node is present **iff** there is exactly one steerable camera
behind the output. A composite output that happens to contain a PTZ tile does **not** advertise PTZ —
the operator drives that camera from the Multiview UI / its own ONVIF endpoint instead. The relay adds
**no engine path**: it is a control-plane SOAP-in → driver-call-out hop, bounded and isolated.

---

## 5. The PTZ standard(s) — ONVIF PTZ + VISCA / VISCA-over-IP

**Operator question: "PTZ — is there a standard?" Answer: yes, two widely-used ones, and Multiview
speaks both behind one internal model.**

### 5.1 ONVIF PTZ service (the IP-CCTV standard)

(verified: ONVIF PTZ Service Specification.) The verbs:

- **`AbsoluteMove`** — move to an absolute pan/tilt/zoom position;
- **`RelativeMove`** — translate relative to the current position without knowing it, optional speed;
- **`ContinuousMove`** — move with a velocity vector (direction + speed) until **`Stop`** (the
  joystick model);
- **Presets** — `SetPreset` / `GetPresets` / `GotoPreset` / `RemovePreset`;
- **PTZ nodes + coordinate spaces** — a PTZ *node* declares the coordinate **spaces** it supports
  (each a space-URI; ONVIF §5.7 defines the standard set: generic/normalized pan-tilt and zoom
  position/velocity spaces). A node implements the standard spaces for whichever movement types it
  supports. The driver reads `GetNodes`/`GetConfigurations` and maps the camera's declared spaces into
  our internal ranges so a normalized command means the same thing on every camera.

### 5.2 VISCA and VISCA-over-IP (the broadcast/PTZ-camera standard)

(verified: VISCA-over-IP camera-control docs; AVer/Sony VISCA-over-IP guides.) VISCA is the
serial/IP camera-control protocol common on broadcast PTZ cameras. **VISCA-over-IP** carries the same
byte commands over UDP/TCP — the common Sony-style profile defaults to **port 52381** with an 8-byte
header carrying a payload type, a **16-bit payload-length field**, and a sequence number, followed by
the VISCA payload and ACK/Completion replies. (The exact header layout is profile-specific — the
parser is pinned to the documented public profile, not assumed.) We implement a **VISCA-over-IP
profile** (and, where a serial bridge exists, the same commands) covering pan/tilt drive + stop,
zoom, absolute/relative position where the camera supports inquiry, and presets — mapped onto the same
internal model as ONVIF PTZ. VISCA is a **de-facto** standard (no single neutral SDO owns the public
spec the way ONVIF does); we build from publicly-documented command tables, **nominative use only**,
bundling no vendor SDK (the clean-room posture, §7 / [ADR-0064](../decisions/ADR-0064.md)).

### 5.3 One internal PTZ model, two profiles

[ADR-0064](../decisions/ADR-0064.md) pins a **vendor-neutral `PtzCommand`** (a closed, typed enum:
`ContinuousMove{pan,tilt,zoom velocities}`, `AbsoluteMove`, `RelativeMove`, `Stop`,
`GotoPreset`, `SetPreset`, `RemovePreset`, `ListPresets`) with **normalized `[-1.0, 1.0]` axes** and a
`PtzNode` capability descriptor (which axes/presets the bound camera supports). Two driver profiles
implement it:

- **ONVIF-PTZ** — translate to/from ONVIF spaces using the node's declared coordinate spaces;
- **VISCA-IP** — translate to/from VISCA byte commands.

The SPA shows the operator **the same PTZ pad / preset list** regardless of which camera is behind the
tile; capabilities the camera lacks are greyed out (never colour-alone — WCAG). This is the typing
discipline of [ADR-M008](../decisions/ADR-M008.md): a closed enum, fixed capability flags, no
property bag.

### 5.4 Where PTZ commands originate (all control-plane)

PTZ verbs reach a camera from three places, all isolated:
1. the **Multiview UI** (operator drives a tile's camera) — REST `POST …/devices/{id}/ptz/*`;
2. a **control surface** (a joystick/MIDI/keypad — see [control-surfaces-midi.md](control-surfaces-midi.md)) mapped to PTZ;
3. the **ONVIF-server PTZ-passthrough relay** (§4.3) — a third-party NVR steering a relayed camera.

All three converge on the same `PtzCommand` → driver. None is on the data plane.

### 5.5 FreeD / position telemetry (forward-reference only)

**FreeD** is a camera-position *telemetry* protocol (pan/tilt/zoom/focus + XYZ as a UDP datagram
stream) used for AR/virtual-set tracking — the *inverse* of PTZ control (it reports where a camera is
pointing, rather than commanding it). It is **out of scope** here and noted only so the PTZ model's
axes are designed to *not preclude* a future FreeD-out facet ([output-metadata-and-orientation.md](output-metadata-and-orientation.md) is the natural home if it is ever built). (unverified: exact FreeD packet layout — not researched for this brief; flagged as an open item.)

---

## 6. Fit to the managed-devices driver model (ADR-M008 / ADR-M009)

Nothing here is a new architecture — it is **two new families inside the shipped Devices domain**.

- **Closed driver enum.** Add `DeviceDriver::Onvif` to the closed `#[non_exhaustive]` enum
  (`crates/multiview-config/src/device.rs:35` — today `Zowietek`/`Displaynode`/`Cast`), wire token
  `"onvif"`, `requires_address = true`. A new family is a new variant + module + ADR — never a plugin
  ([ADR-M008](../decisions/ADR-M008.md)). VISCA cameras that are *also* ONVIF are one `onvif` device
  with a VISCA-IP PTZ profile selected; a VISCA-only camera (no ONVIF) is a thin variant decision
  noted in [ADR-0064](../decisions/ADR-0064.md) (likely a `visca` PTZ-only device or a flag on a
  generic-camera device — an open question, §8).
- **Fixed projections + a fourth facet.** Source candidates (RTSP profiles), output targets (an ONVIF
  camera generally has no decode side, so usually none), display heads (none), **plus PTZ** as a new
  fixed facet on the driver capability flags (`{encode, decode, display, sync, audio, reboot,
  firmware_update}` gains a `ptz` flag, [ADR-M008](../decisions/ADR-M008.md) §2.3 capability set).
- **Lifecycle + reconnect for free.** `ADOPTING → ONLINE / DEGRADED / AUTH_FAILED / UNREACHABLE`
  (`state_machine.rs`) and supervised reconnect (the `zowietek`/`cast` precedent) apply unchanged. On
  reconnect the driver re-converges desired state (e.g. a pending `SetVideoEncoderConfiguration`).
- **Config-as-code.** A `[[devices]] driver = "onvif"` record with `address`, `auth.secret_ref`,
  and a `ptz_profile = "onvif" | "visca-ip"` selector; runtime status (online, firmware, current
  profile, PTZ position) is the read-only projection, never exported
  ([managed-devices.md §7.3](managed-devices.md)).
- **Realtime.** Rides the existing `Topic::Devices` ([ADR-RT007](../decisions/ADR-RT007.md)):
  `device.status` (conflated; add a PTZ-position sub-field — latest-wins, like temperature),
  `device.adopted`/`.removed`/`.error`, and `device.discovered` for WS-Discovery rows. **No new
  topic** (coarse-topic + ids rule).
- **API surface** (all under `/api/v1`, OpenAPI-first, bare-verb routes per
  [ADR-W017](../decisions/ADR-W017.md), the device routes block `routes/mod.rs:672`):

```
POST   /discovery/devices/scan          # extended: includes a WS-Discovery browse (Onvif kind)
POST   /devices/probe-onvif             # cross-subnet test: GetSystemDateAndTime + GetCapabilities (no record)
GET    /devices/{id}/ptz                # PtzNode capabilities + last-known position
POST   /devices/{id}/ptz/move           # ContinuousMove / AbsoluteMove / RelativeMove (typed body) → 202/200
POST   /devices/{id}/ptz/stop           # Stop
GET/POST /devices/{id}/ptz/presets      # list / set
POST   /devices/{id}/ptz/presets/{p}/goto
POST   /devices/{id}/onvif/set-time     # SetSystemDateAndTime (sync to Multiview/NTP) — DEV-class
POST   /devices/{id}/onvif/encoder      # SetVideoEncoderConfiguration — DEV-class (declared impact)
```

Long-running / device-reset ops return `202` + an operation id with the outcome on the realtime
stream (the established pattern); ETag/If-Match on the record; `Idempotency-Key` on actions; problems
are RFC 9457. ContinuousMove/Stop are fast Class-1-ish control actions (`200`); encoder/time changes
are `DEV`-class with a declared device-side impact ([managed-devices.md §10](managed-devices.md)).

---

## 7. Security, IPv6, efficiency, and vendor posture

**Security.**
- **Discovery is untrusted + bounded.** WS-Discovery responders are unauthenticated and spoofable;
  results are confirm-adopt candidates only ([ADR-0041](../decisions/ADR-0041.md)), the inventory is
  hard-capped + rate-limited + responder-string-truncated (the shipped discovery hardening). The
  ONVIF-server responder answers Probes but exposes **nothing** without auth beyond identity scopes.
- **Auth everywhere.** WS-Security UsernameToken digest (or HTTP Digest) on both client and server
  facets; credentials are write-only `secret_ref`s, never echoed. Plain-HTTP is a management-VLAN
  recommendation with an explicit warning, never the default — the `zowietek` posture.
- **PTZ is a privileged action.** Relayed PTZ (the ONVIF-server passthrough) is authorised against
  the control-plane authz (`auth.rs`/object-level checks); a third-party NVR cannot steer a camera it
  has not been granted. PTZ verbs are rate-limited per device (serialise per device, the `zowietek`
  per-device serialisation rule) so a joystick spam cannot DoS a camera.
- **TLS.** HTTPS to ONVIF endpoints where the camera supports it; cameras with self-signed certs are
  handled with an explicit operator trust decision (not blanket verification-off — unlike Cast's
  ecosystem-wide exception, ONVIF cameras can run real certs on a management VLAN).

**IPv6-first** ([ADR-0042](../decisions/ADR-0042.md)). WS-Discovery sends the IPv6 Probe and joins
`ff02::c` first; manual endpoints lead with bracketed IPv6 literals; the ONVIF-server facet binds
dual-stack `[::]` (`IPV6_V6ONLY=false`), advertises bracketed IPv6 stream URIs, and emits SDP/`c=IN
IP6` for RTP where applicable. Many fielded cameras are **IPv4-legacy** end-to-end — handled as a
legacy-interop island under [conventions §10](../architecture/conventions.md), exactly as the
`zowietek` IPv4-only devices are ([managed-devices.md §3.4](managed-devices.md)); all
Multiview-side surfaces stay IPv6-first.

**Efficiency budget (mem / cpu / gpu / io).**
- **GPU: zero.** No ONVIF/PTZ path touches the compositor or any GPU.
- **CPU: negligible.** SOAP serialise/parse + a ≤1 Hz status poll per camera, on the camera's own
  driver task; PTZ commands are operator-rate. Server-facet SOAP handling is request-rate (an NVR
  polls metadata occasionally; the actual video is the *existing* RTSP serve at *existing* cost —
  encode-once-mux-many, invariant #7, no new encode).
- **Memory: bounded.** The discovery inventory is hard-capped (existing bound); each driver holds one
  small typed status + capability struct; SOAP buffers are size-capped (a hostile/oversized SOAP body
  is rejected, the discovery zlib/size-cap discipline of [ADR-0041](../decisions/ADR-0041.md)
  generalised to SOAP). No per-frame allocation anywhere — there are no frames on this path.
- **IO: a handful of small UDP multicast packets per scan + per-camera unicast SOAP at poll cadence;**
  PTZ is a small UDP/TCP datagram per command (VISCA) or one SOAP call (ONVIF). All bounded.

**Vendor / license posture** (mirrors [managed-devices.md §12](managed-devices.md)).
- **ONVIF is an open standard.** We implement the published SOAP/WSDL service shapes from the ONVIF
  specifications and OASIS WS-Discovery; we **do not** bundle gSOAP, a vendor ONVIF toolkit, or any
  vendor camera SDK. `unsafe_code = forbid`, LGPL-clean default build, off-by-default network feature.
- **We do not redistribute the ONVIF spec text** — endpoint descriptions in this repo are original
  re-expressions of the documented operations. ONVIF conformance/logo claims require the paid
  certification program; we claim **interoperability**, not certification (and say so).
- **VISCA** is a de-facto vendor protocol with publicly-documented command tables; we implement
  VISCA-over-IP from public documentation, **nominative use only**, no vendor SDK, with a
  non-affiliation disclaimer — the same clean-room framing the `zowietek`/Cast drivers carry. Defer to
  the operator and counsel on any specific vendor's protocol-terms stance (the pending counsel review,
  [managed-devices.md §12](managed-devices.md)).

---

## 8. Open questions

1. **VISCA-only device modelling.** Is a VISCA-only PTZ camera (no ONVIF, no RTSP — control-only) its
   own `DeviceDriver::Visca` variant, or a PTZ-profile flag on a generic-camera device? Leaning: a
   thin `visca` variant whose only facet is PTZ, so the closed-enum/fixed-flags discipline holds — but
   this is genuinely a modelling choice for [ADR-0064](../decisions/ADR-0064.md), not yet pinned.
2. **ONVIF-server conformance depth.** Exactly which Device/Media2 operations must a *generic* NVR
   (not a certified test tool) call to discover-and-pull our output? We target the practical subset;
   the gap to full Profile-S/T conformance is documented, not closed. Some NVR onboarding flows may
   additionally expect an **Events** service or a **Media1 (`ver10`) fallback** before they will
   complete adoption; whether to add either is decided by what the validation NVRs actually require.
   Until hardware validation against ≥2 real NVRs/VMS (the gate, §9) passes, the facet claims
   **best-effort interoperability**, not generic-NVR compatibility — the qualification gate must not
   overpromise.
3. **PTZ coordinate-space normalization fidelity.** ONVIF cameras may expose only device-specific
   spaces (not the generic normalized spaces). Mapping a vendor space to our `[-1,1]` axes may be
   approximate for AbsoluteMove on such cameras; ContinuousMove (velocity) is robust. How honest a
   "absolute position unavailable / approximate" badge do we surface? (UI honesty rule.)
4. **PTZ-passthrough ambiguity at the edges.** A single-tile output whose bound camera is *swapped*
   live (crosspoint re-point, [decoupled-routing.md](decoupled-routing.md)) changes which camera the
   relayed PTZ steers. Do we (a) re-target the relay seamlessly, (b) drop the PTZ node during the
   swap, or (c) refuse swap while an NVR holds a PTZ session? Leaning (a) with a brief node-absent
   blip — but it needs the routing seam's confirmation.
5. **Audio.** ONVIF Profile-S/T includes audio backchannel and G.711/AAC audio; do we ingest ONVIF
   camera audio as an ordinary audio source candidate (likely yes, via the same RTSP profile) and do
   we ever advertise audio on the server facet? Audio-in is cheap (reuse the audio source path); audio
   backchannel (talk-down to a camera) is out of scope unless asked.
6. **WS-Discovery scope filtering.** Should the operator be able to scope-filter discovery (by
   profile/location/hardware) to tame a large camera fleet, or is the bounded cap + confirm-adopt
   enough? Default: cap + manual; scoped Probe is a later refinement.
7. **FreeD-out.** Worth a future facet for AR/virtual-set users? Out of scope now; the PTZ axis model
   is designed not to preclude it (§5.5).

---

## 9. Hardware validation (gates the claims, not the build)

| Target | Validates |
|---|---|
| ≥2 real ONVIF cameras (different vendors, ideally one Media1-only + one Media2) | WS-Discovery probe/match, WS-Security clock-skew handling, `GetProfiles`/`GetStreamUri` ingest, `SetSystemDateAndTime`, `SetVideoEncoderConfiguration` device-reset behaviour |
| ≥1 ONVIF PTZ camera + ≥1 VISCA-over-IP PTZ camera | the two PTZ profiles against the one internal model; preset round-trips; ContinuousMove/Stop latency; coordinate-space mapping fidelity |
| ≥2 third-party NVR/VMS clients | the ONVIF-server facet: discovery, auth, profile listing, RTSP pull of a Multiview `RtspServer` output, and PTZ passthrough on a single-camera output |
| The cross-subnet test path | manual endpoint add across a router (multicast does not cross); test → adopt; the three failure distinctions (unreachable / not-ONVIF / auth-failed) |

Socket-free unit tests (the `tower::oneshot` posture) cannot catch camera-vs-spec drift — ONVIF
cameras are notoriously non-conformant — so each profile carries a mandatory on-hardware validation
item before its typed client is trusted (the `zowietek` doc-vs-firmware lesson,
[managed-devices.md §13](managed-devices.md)).

---

## 10. External references

- **ONVIF Core Specification / Application Programmer's Guide** — WS-Discovery usage,
  `NetworkVideoTransmitter` type, the unauthenticated `GetSystemDateAndTime`-before-auth model.
  (web-verified: ONVIF APG; OASIS WS-Discovery 1.1.)
- **OASIS WS-Discovery 1.1** — SOAP-over-UDP multicast `239.255.255.250:3702` / IPv6 `ff02::c`,
  Hello/Probe/ProbeMatch. (web-verified.)
- **ONVIF Device Service** — `GetDeviceInformation`, `GetCapabilities`/`GetServices`,
  **`SetSystemDateAndTime`** (mandatory in Profile S). (web-verified: ONVIF Profile-S spec.)
- **ONVIF Media / Media2 Service Specification** — `GetProfiles`, video-encoder configuration,
  `GetStreamUri`. (web-verified: ONVIF Media2 spec.)
- **ONVIF Imaging Service** — imaging settings + focus Move/Stop. (unverified: exact op set not
  web-fetched this pass; cited from spec family knowledge.)
- **ONVIF PTZ Service Specification** — AbsoluteMove / RelativeMove / ContinuousMove / Stop, presets
  (SetPreset/GetPresets/GotoPreset/RemovePreset), PTZ nodes + coordinate spaces (§5.7). (web-verified.)
- **ONVIF Profile S / G / T / M scopes** — Profile S (streaming, H.264/RTSP, PTZ), T (H.265 +
  advanced), M (metadata/analytics), G (recording/storage). (web-verified: Profile-S spec; others
  from profile-family knowledge — exact M/G boundaries unverified this pass.)
- **VISCA / VISCA-over-IP** — camera-control byte protocol; VISCA-over-IP commonly UDP/TCP **52381**,
  8-byte header with a 16-bit payload-length field + sequence number, ACK/Completion. (web-verified: AVer/Sony VISCA-over-IP
  guides; de-facto vendor standard, no neutral SDO.)
- **FreeD** — camera-position telemetry (forward-reference only; packet layout unverified).
- **NDI PTZ** — NDI's own PTZ control surface (out of scope here; the `ndi` feature is a separate
  runtime-loaded path — [docs/io/ndi.md](../io/ndi.md)).

## Decision records

- [ADR-0062 — ONVIF client + WS-Discovery + manual cross-subnet endpoint add](../decisions/ADR-0062.md)
- [ADR-0063 — ONVIF server: present Multiview outputs as ONVIF/RTSP devices; PTZ passthrough](../decisions/ADR-0063.md)
- [ADR-0064 — PTZ control model: ONVIF PTZ service + VISCA / VISCA-over-IP profile](../decisions/ADR-0064.md)

Owned by the sibling Devices brief [managed-devices.md](managed-devices.md):
[ADR-M008](../decisions/ADR-M008.md), [ADR-M009](../decisions/ADR-M009.md),
[ADR-M010](../decisions/ADR-M010.md), [ADR-RT007](../decisions/ADR-RT007.md),
[ADR-W017](../decisions/ADR-W017.md); discovery doctrine [ADR-0041](../decisions/ADR-0041.md).
