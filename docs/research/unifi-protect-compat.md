# Multiview — UniFi Protect Camera Compatibility (Outputs Adoptable as Cameras)

**Area:** Output / Control (ONVIF-server facet) / Web
**Status:** Design brief (Proposed) — docs-only; implementation follows in dependency-ordered waves.
**Drives:** [ADR-0065](../decisions/ADR-0065.md) (UniFi Protect compatibility — present outputs as adoptable cameras over ONVIF/RTSP + a clean-room nominative adoption shim).
**Extends:** [onvif-and-ptz.md](onvif-and-ptz.md) (the ONVIF-server facet + ADR-0063, the load-bearing prerequisite), [managed-devices.md](managed-devices.md) (the clean-room nominative vendor posture this mirrors), [ADR-0006](../decisions/ADR-0006.md) (in-process RTSP serving), [ADR-0037](../decisions/ADR-0037.md) (best-effort sinks that cannot back-pressure the engine).
**Backlog:** `UPROT-*` in [`../development/feature-intake-2026-06-13.md`](../development/feature-intake-2026-06-13.md).

> **Framing.** The operator asks for "full emulation / compatibility for Multiview outputs to be
> used by a UniFi Protect NVR." This brief's honest headline: **as of UniFi Protect 5.0+ this is
> almost entirely the generic ONVIF-server facet plus a tiny compatibility profile — not a bespoke
> emulation of any vendor protocol.** A composited Multiview output (program, a clean feed, a single
> tile, or a wall head) should present itself to a Protect controller as an ordinary ONVIF Profile S
> network camera over RTSP, and Protect adopts it the same way it adopts any third-party ONVIF camera.
> Everything beyond that — Protect-native smart detections, two-way audio, the proprietary AI-port /
> G-series enrollment path — is **vendor-proprietary, clean-room-only, and deferred to operator
> decision.** This brief is deliberately short and heavily caveated.

---

## 0. Headlines

1. **The bulk is ADR-0063, not new work.** UniFi Protect 5.0 (controller build `5.0.x`, mid-2024)
   added the ability to adopt third-party **ONVIF/RTSP** cameras; 6.x keeps the same path with the
   same limitations (web-verified, §2). If Multiview ships the generic **ONVIF Profile S server facet**
   ([onvif-and-ptz.md](onvif-and-ptz.md) / ADR-0063) wrapping the **in-process RTSP
   server** ([ADR-0006](../decisions/ADR-0006.md); expected seam at `crates/multiview-output/src/rtsp_server/`),
   a Protect NVR should adopt a Multiview output with **no Protect-specific code at all**. This brief
   should say that plainly and resist inventing a "Protect emulator."
2. **What's left is a thin compatibility profile, not an emulation.** The only Protect-specific work
   should be (a) a documented **adoption profile** — the exact ONVIF ports, auth mode, stream/snapshot
   shapes Protect's adoption flow expects (`Digest + WS-UsernameToken`, the `:8000` ONVIF service
   port convention, a working `GetSnapshotUri`); and (b) an interop test matrix run against a real
   Protect controller. No reverse-engineered protocol.
3. **Outputs become camera "channels," never Sources.** A Multiview output is offered to Protect as
   an ONVIF media profile (main + sub stream, mirroring Protect's high/low auto-detect). This reuses
   the **Devices mental model inverted** ([managed-devices.md](managed-devices.md)): there, a device
   *projects* candidates into Multiview; here, Multiview *projects* its outputs out to an external NVR
   as adoptable camera channels. The engine is untouched — encode-once-mux-many (#7) feeds the existing
   RTSP fan-out seam.
4. **Invariant #1 is the whole point and is free here.** A Protect controller is just another RTSP
   client. It pulls; it never paces. The ONVIF control surface is a control-plane HTTP/SOAP server that
   lives beside the engine (the NMOS-module isolation shape, `crates/multiview-control/src/nmos/mod.rs`),
   never on the output-clock loop. A Protect NVR that hangs, disconnects, or hammers re-adoption can at
   worst stall its own RTSP session — the program output never falters (§4, invariants #1/#10).
5. **The proprietary path is a flagged non-goal pending operator sign-off.** Protect's *native* camera
   adoption (the AI-port / G-series enrollment + UniFi cloud/inform protocol) is proprietary, undocumented,
   and certificate/account-bound. We describe it **only at the behavioral level**, never redistribute or
   reverse-engineer it, and **defer the stance to the operator** (§4.3, §5). The open ONVIF path is the
   recommended and sufficient answer to the operator's request.

---

## 1. Posture: open-standards-first + clean-room nominative (the managed-devices model)

This brief inherits the exact vendor posture proven in [managed-devices.md §12](managed-devices.md)
(the ZowieTek/Cast clean-room model) and [CODE_OF_CONDUCT.md](../../CODE_OF_CONDUCT.md):

- **Build on open standards.** ONVIF (Profile S core media), RTSP (RFC 7826/RFC 2326), RTP/RTCP, and
  SDP are open, published, royalty-free-to-implement specifications. The entire adoptable-camera surface
  is buildable from these alone — no vendor SDK, no vendor spec.
- **Nominative use only.** "Compatible with UniFi Protect (third-party ONVIF camera adoption)" in plain
  text, with a non-affiliation disclaimer: *"UniFi and UniFi Protect are trademarks of Ubiquiti Inc.
  This project is independent and not affiliated with or endorsed by Ubiquiti."* Never a Ubiquiti logo,
  trade dress, "Works with UniFi" badge, or any implied certification.
- **No spec/SDK redistribution.** We never commit Ubiquiti documentation text, screenshots, or any
  Protect/UniFi-OS binary or protocol capture. The behavioral facts in §2 are original re-expressions
  of publicly observable adoption behavior and vendor help-center descriptions (cited, §6) — written,
  not copied.
- **Proprietary protocol = behavioral-only + deferred.** Any Protect-native (non-ONVIF) adoption path
  is documented strictly at the behavioral level, flagged with legal/ToS caveats, and its
  implementation stance is **an operator decision** (§5). The default and recommended answer uses only
  open standards.

This is the same residual-risk-accepted, removable-driver, vendor-neutral-abstraction discipline the
Devices domain already carries; the ONVIF facet is just another conformant server, not a vendor shim.

---

## 2. What UniFi Protect can adopt today (verified)

The load-bearing factual question — *does Protect adopt generic ONVIF/RTSP cameras?* — verified via
the Ubiquiti help center and independent third-party testing (§6):

- **Yes, since UniFi Protect 5.0.** Third-party **ONVIF** camera adoption shipped in the Protect `5.0.x`
  application generation (mid-2024); **6.x retains the same path and the same constraints**. Before 5.0,
  third-party RTSP was effectively unsupported in the Protect app. *(Exact point-release where it became
  GA varies by report (~`5.0.3`); treat the precise build as an interop-test pin, not a spec.)*
- **Adoption mechanism (behavioral).** Protect discovers ONVIF cameras on the **same VLAN/subnet**
  automatically; for off-subnet or non-discovered devices, the operator uses **"advanced adoption"** and
  supplies the camera's IP with the **ONVIF service port appended** (the `:8000` convention is what
  Protect's UI prompts for). Adoption requires ONVIF enabled on the camera with an **ONVIF
  username/password**, with authentication as **Digest + WS-UsernameToken**, and a correctly set
  **camera clock/timezone** (WS-Security timestamps fail otherwise).
- **Stream selection (behavioral).** Protect auto-detects the **highest- and lowest-quality** ONVIF
  media profiles the camera advertises and maps them to its main/sub model; the operator **cannot pick**
  which profile maps where in the Protect UI (a documented Protect limitation), so the *camera* must
  advertise sensible main/sub profiles. Changing profiles after adoption requires **remove + re-add**.
- **Snapshot.** Protect has historically needed a working **ONVIF snapshot** (`GetSnapshotUri` → an
  `image/jpeg` URL); newer builds improved handling of cameras *without* snapshot, but a working snapshot
  endpoint is the safe target.
- **Hard limitations on third-party cameras (verified, both 5.x and 6.x).** Third-party ONVIF cameras
  get **continuous recording + live view only**: **no Protect smart/AI detections, no motion-event
  recording parity, no two-way audio, no package/person/vehicle detection** — those remain
  UniFi-native-camera features. This **bounds the operator's expectation**: "used by a UniFi Protect NVR"
  realistically means *recorded and viewed*, not *fully feature-equivalent to a native UniFi camera*.

**Implication.** The operator's request is satisfied by Multiview presenting a **conformant ONVIF
Profile S camera** whose media is the existing RTSP output. There is no Protect-proprietary protocol to
emulate for the recording/live-view use case. The honest scope is "ONVIF facet + a Protect profile +
interop tests," and this brief says exactly that rather than over-promising a full emulation.

---

## 3. Multiview-output-as-camera over ONVIF/RTSP (reuse ADR-0063)

The adoptable-camera surface is the generic **ONVIF-server facet** owned by
[onvif-and-ptz.md](onvif-and-ptz.md) / ADR-0063, plus the in-process RTSP server. This brief adds
**only** the Protect compatibility profile on top; the heavy lifting is reuse.

### 3.1 Existing design dependencies (expected seams — to verify before implementation)

These are the existing/expected seams this brief consumes. They are stated as design dependencies;
the exact module paths and symbols are expected shapes to confirm at implementation time, not a claim
of present-tree status.

- **In-process RTSP server.** `crates/multiview-output/src/rtsp_server/` — the typed seam
  (`RtspServerSink`, `RtspMount`, `BoundedPacketQueue`) plus the `gst-rtsp-server` serving thread
  behind the off-by-default `rtsp-server` feature (`RtspServerHandle::start`), with the mount→URL
  joiner in `crates/multiview-output/src/rtsp.rs`. This is the [ADR-0006](../decisions/ADR-0006.md)
  primary path; fan-out is encode-once (#7) via `crates/multiview-output/src/fanout.rs`.
- **JPEG still seam (snapshot building block).** `crates/multiview-control/src/routes/preview.rs`
  is expected to serve `GET /api/v1/preview/program.jpg` → `image/jpeg` from a pure-Rust NV12→JPEG
  encoder (`crates/multiview-preview/src/encode.rs`). An ONVIF `GetSnapshotUri` should resolve to an
  equivalent still **for the offered output** (not the preview tap); the encoder and the no-store
  image response pattern are the reusable building blocks.
- **Control-plane server isolation precedent.** `crates/multiview-control/src/nmos/mod.rs` (model
  compiled always; `nmos_router()` behind the feature) is the shape an ONVIF SOAP server should
  follow: a control-plane HTTP server beside the engine that the engine never awaits.
- **Action-route style + discovery confirm-adopt.** Bare verb segments (`/alarms/{id}/ack`,
  `/salvos/{id}/arm`) in `crates/multiview-control/src/routes/mod.rs`; the untrusted-inventory
  confirm-adopt discipline in `crates/multiview-control/src/routes/discovery.rs` ([ADR-0041](../decisions/ADR-0041.md)).

### 3.2 What ADR-0063 (the ONVIF facet) should provide, that this brief consumes

The ONVIF facet (owned by [onvif-and-ptz.md](onvif-and-ptz.md)) should expose, per offered output:

- **ONVIF Profile S device + media services** over SOAP/HTTP on a configurable service port (default the
  conventional `:8000` Protect prompts for): `GetDeviceInformation`, `GetCapabilities`/`GetServices`,
  `GetProfiles`, `GetStreamUri` (→ the existing `rtsp://…/mount` for that output), `GetSnapshotUri`
  (→ a per-output JPEG still), `GetSystemDateAndTime` (Protect checks clock skew). The snapshot itself
  must be **sampled and cache-based**: served from the most recent last-good output frame on a bounded,
  rate-limited control-plane path (one cached JPEG per offered camera, re-encoded at most at a capped
  cadence), so Protect thumbnail polling can never synchronously touch the output-clock loop (#1) or
  spike CPU per request — repeated polls return the cached still, not a fresh on-demand encode.
- **WS-Discovery** (`urn:schemas-xmlsoap-org:ws:2005:04:discovery`) responder so a same-VLAN Protect
  controller auto-finds the offered cameras; bounded/rate-limited responder listening on the
  well-known WS-Discovery multicast groups over **UDP/3702** — the standard ONVIF discovery model
  (ASM/any-source multicast, **not** SSM): IPv4 `239.255.255.250` and/or IPv6 `FF02::C`, with
  Protect interop deciding which is enabled by default under [conventions §10](../architecture/conventions.md).
- **Auth**: HTTP **Digest** + WS-Security **UsernameToken** (Protect's required mode), credentials from
  the write-only secret store (the `secret_ref` pattern, `crates/multiview-config/src/schema.rs`).
- **Two media profiles per camera** (a "main" + a "sub") mapped to two Multiview renditions, so Protect's
  high/low auto-detect maps cleanly — this is encode-once-mux-many fan-out (#7), not extra encodes when
  the renditions already exist. **Config validation must require that `main_rendition`/`sub_rendition`
  name renditions that already exist**: it is free fan-out only when they do. If a named rendition does
  not exist, that is a new sub/main encode and is **not** free — it must enter the normal admission /
  resource-budget path (inv #9), not be silently spun up by the ONVIF facet; the validator rejects or
  flags the added encode cost rather than over-committing the engine.

### 3.3 The Multiview-specific projection: which outputs become cameras

A new config axis (additive, like ISO/Program recording in [ADR-0037](../decisions/ADR-0037.md)):

```toml
[[outputs.onvif_camera]]            # offer this output as an adoptable ONVIF camera
enabled       = true
service_port  = 8000               # ONVIF service port (Protect's :8000 convention)
profile       = "unifi-protect"    # the compatibility profile (§3.4); "generic" otherwise
main_rendition = "program-1080p"   # → ONVIF "main" media profile
sub_rendition  = "program-360p"    # → ONVIF "sub" media profile (Protect high/low auto-detect)
[outputs.onvif_camera.auth]
secret_ref = "op://Site/multiview-onvif/credentials"   # Digest + WS-UsernameToken
```

A camera channel can front the **program**, a **clean feed**, a **single tile/source as a cropped
output**, or a **wall head** — each is an ordinary Multiview output; offering it as ONVIF only adds the
SOAP description + discovery advert. Class-1 (a best-effort egress facet toggling), surfaced via the
capability matrix.

### 3.4 The "unifi-protect" compatibility profile (the only Protect-specific code)

A small named profile that constrains the generic ONVIF responses to what Protect's adoption flow is
known to accept (all behavioral, all derivable from §2):

- advertise exactly **two** media profiles (main/sub) with sane, distinct resolutions/bitrates so
  Protect's high/low mapping is unambiguous;
- guarantee a working **`GetSnapshotUri`** (Protect's safe path);
- **H.264** main profile for the video media (broadest Protect/codec coverage; HEVC is an interop-test
  axis, not a default). The interop matrix pins the concrete constraints Protect is tested against —
  profile/level, SPS/PPS-in-band cadence, IDR/keyframe interval, monotonic re-stamped timestamps
  (inv #3), and the offered RTSP transport modes — rather than leaving "main profile" underspecified;
- **RTP/AVP over RTSP/TCP** interleaved as the offered lower transport (NVRs default to TCP pull);
- correct **`GetSystemDateAndTime`** + WS-Security timestamps within Protect's skew tolerance;
- conservative ONVIF capability advertisement — **do not** advertise analytics/events/PTZ the output
  cannot honor (avoid Protect probing a capability we then fail). PTZ, if ever offered, is the
  [onvif-and-ptz.md](onvif-and-ptz.md) facet's concern, not this profile's.

That is the entire Protect-specific surface: a profile object + an interop test matrix. **No emulation
of any Ubiquiti protocol.**

---

## 4. The clean-room adoption shim (behavioral only; legal/ToS caveats)

### 4.1 The open path needs no "shim"

For ONVIF/RTSP adoption there is **no proprietary handshake** — Protect speaks standard ONVIF to the
camera. The "shim" is therefore just the §3.4 compatibility profile + conformance to Protect's documented
expectations. We recommend shipping exactly this and nothing more for the operator's stated goal.

### 4.2 Isolation by construction (invariants #1/#10)

- **#1 (output never stalls).** A Protect NVR is an RTSP *client* of the existing server; it pulls frames
  the engine already produced. It cannot pace the output clock — `out_pts = f(tick)` is upstream of and
  oblivious to any RTSP session. The ONVIF SOAP server is a control-plane HTTP server (NMOS-module shape)
  with no path to the tick loop.
- **#10 (no back-pressure).** RTSP egress uses the existing `BoundedPacketQueue` drop-oldest fan-out
  (`crates/multiview-output/src/rtsp_server/`), so a slow/greedy Protect session drops frames on *its*
  queue, never blocks the bake-consumer or the engine. The ONVIF server publishes/reads only
  control-plane state via watch/broadcast channels; the engine never `.await`s it. A Protect controller
  re-adopting in a loop, or 4 Protect controllers pulling the same camera, is bounded fan-out — the CI
  chaos gate that already guards #10 over the RTSP/NMOS channels covers this.
- **Global resource bounds (process self-protection).** Drop-oldest per session isolates the *engine*,
  but an adoption loop or many NVRs could still exhaust *process* resources. The ONVIF/RTSP facet must
  therefore carry explicit, configurable caps — max concurrent RTSP sessions (overall and per offered
  camera), a bounded accept queue, SOAP/WS-Discovery request rate limits, auth-attempt throttling, and
  a fixed fan-out/snapshot memory + fd budget — all enforced on the control-plane side, never by
  back-pressuring the bake-consumer. Exhausting these caps rejects new clients (control-plane error);
  it can never stall an existing RTSP session or the engine. These bounds are a chaos-test criterion
  (open question for exact defaults, set during ADR-0063 implementation).

### 4.3 The proprietary native-adoption path (behavioral description only — DEFERRED)

UniFi-native cameras (the G-series and the AI-port-attached models) are **not** adopted over ONVIF. From
publicly observable behavior only:

- native cameras enroll into the Protect controller over a **proprietary UniFi inform/adoption protocol**
  (the same family as UniFi Network device adoption), typically TLS-secured and tied to controller-issued
  credentials/certificates and, for some models, a hardware **AI port** that bridges the camera to the
  controller; smart detections run on the camera/AI-port silicon and report results over this proprietary
  channel.
- This path is **undocumented, proprietary, certificate/account-bound, and governed by Ubiquiti's ToS.**
  Emulating it to make a Multiview output appear as a *native* UniFi camera (and thus unlock smart
  detections) would require reverse-engineering a closed protocol.

**Stance (deferred to operator, §5):** Multiview should **not** implement or emulate the proprietary
native path by default. It is described here at the behavioral level for completeness and flagged with
legal/ToS caveats; whether to pursue any of it is an explicit operator decision, not an engineering
default. The open ONVIF path already satisfies "used by a UniFi Protect NVR" for recording and live view.

### 4.4 Legal / ToS caveats (carry into counsel review)

- Implementing a **standards-conformant ONVIF/RTSP server** that a third-party NVR chooses to adopt is
  ordinary interop and squarely within the [managed-devices §12](managed-devices.md) clean-room posture.
- **Do not** redistribute Ubiquiti specs/SDK/firmware or capture/replay their proprietary protocol.
- Nominative trademark use only (§1); no certification claim; no "Works with UniFi" badge.
- Ubiquiti's terms and Protect's third-party-camera support are **revocable and may change per release**
  — pin interop to tested Protect builds and treat the compatibility profile as a *removable* facet
  (same removable-driver logic as the Devices domain). Folds into the already-pending counsel review.

---

## 5. Open questions / operator decision points

1. **Proprietary native path — pursue at all?** Recommended default: **no** (ONVIF-only). Operator
   decision: do we ever want to investigate the native UniFi adoption/AI-port path, even behaviorally,
   given the ToS/clean-room caveats (§4.3)? Until then it stays a documented non-goal.
2. **Snapshot scope.** Should `GetSnapshotUri` serve a still of the **offered output** specifically
   (new per-output JPEG tap) vs. reuse the program preview JPEG seam (`routes/preview.rs`)? Lean: per-output, so a
   tile/clean-feed camera snapshots correctly — but that is a small new tap, named here, not assumed.
3. **Which outputs default to "camera"?** None by default (explicit opt-in per §3.3), or program-on by
   default when a `onvif_camera` block is absent but a Protect-compat flag is set? Lean: explicit opt-in
   (least surprise; avoids advertising every rendition on the network).
4. **Audio.** Protect third-party audio support is inconsistent across builds; do we advertise an audio
   track on the ONVIF camera (program bus AAC) or stay video-only for v1? Lean: video-only v1, audio an
   interop-test axis (mirrors managed-devices' conservative claims).
5. **Codec floor.** H.264 main-profile default is safest; is HEVC adoption into Protect worth an interop
   pass, or explicitly out of scope for v1? Lean: H.264 v1, HEVC measured-not-promised.
6. **Discovery blast radius.** WS-Discovery makes every offered output visible to anything on the VLAN.
   Default to **discovery off / manual advanced-adoption only**, or on? Lean: off by default; opt-in per
   camera, IPv6-first, bounded responder — consistent with [ADR-0041](../decisions/ADR-0041.md)'s
   untrusted-inventory caution applied outbound.

---

## 6. External references (verified unless marked)

- **Ubiquiti Help Center — "Third-Party Cameras in UniFi Protect"** — the ONVIF/RTSP adoption path,
  `:8000` ONVIF service port, Digest + WS-UsernameToken auth, clock/timezone requirement, and the
  third-party limitations (continuous recording + live view; no smart/AI detections, no two-way audio).
  *(web-verified, 2026-06-13)*
- **Independent third-party testing (NAS Compares; The Smart Home Hook Up; Flytec Computers; IP Cam
  Talk)** — corroborate Protect 5.0 as the version that added third-party ONVIF adoption, the
  highest/lowest auto-detect with no operator profile choice, the remove-and-re-add-to-change-profile
  behavior, and that 6.x keeps the same constraints. *(web-verified, 2026-06-13)*
- **ONVIF Core / Profile S specifications** — `GetProfiles`, `GetStreamUri`, `GetSnapshotUri`,
  `GetSystemDateAndTime`, WS-Discovery, WS-Security UsernameToken. *(open standard; cited by name)*
- **RFC 7826 (RTSP 2.0) / RFC 2326 (RTSP 1.0); RTP/RTCP (RFC 3550); SDP (RFC 8866)** — the media
  transport the offered camera serves. *(open standards; cited by name)*
- **(unverified, behavioral only)** UniFi-native (G-series / AI-port) cameras enroll over a proprietary
  UniFi inform/adoption protocol (TLS, controller-issued credentials), distinct from ONVIF — described
  in §4.3 for completeness; not a target.

## Decision records

- [ADR-0065 — UniFi Protect compatibility: present outputs as adoptable cameras over ONVIF/RTSP + a
  clean-room nominative adoption shim](../decisions/ADR-0065.md)

Prerequisite, owned by the sibling brief [onvif-and-ptz.md](onvif-and-ptz.md):
ADR-0063 (the generic ONVIF Profile S server facet) is the dependency this brief builds the Protect
compatibility profile on top of.
