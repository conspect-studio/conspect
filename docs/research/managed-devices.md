> **Design brief — Managed devices.** Authoritative research/design record for the unified
> **Devices domain**: the device registry, the compiled-in `zowietek` / `displaynode` / `cast`
> drivers, mDNS discovery, and sync groups. Produced by a verification-hardened multi-agent
> research workflow (2026-06-10). This is a **design for an unbuilt feature** — none of the
> modules described here exist yet; every reference to *existing* code names a real, verified
> path. Canonical naming lives in [docs/architecture](../architecture/); ADRs derived from this
> brief are in [docs/decisions](../decisions/). The sibling brief
> [display-out](display-out.md) owns the engine-side display sink and the node presentation
> discipline; this brief owns everything control-plane.

---

# Multiview — Managed Devices (Devices domain)

**One mental model: devices are the hardware inventory; Sources, Outputs, and Layouts remain the
media graph; adopting a device populates the pickers.** A device is never itself a Source or an
Output — it *projects* source candidates, output targets, and display heads into the existing
pickers. Every driver is a control-plane poller/actor; the engine only ever sees ordinary Sources
and Outputs. That single sentence is the invariant-#10 proof for the whole domain.

> **Vendor posture.** This brief names ZowieTek and Google nominatively and factually
> ("works with…"); Multiview is independent of and not affiliated with or endorsed by either.
> All endpoint and behaviour descriptions below are original re-expressions written from the
> vendor-published *ZowieTek API documentation v1.5 (2025)* and from Google's open-sourced
> Open Screen protocol sources — no vendor documentation text is reproduced here or anywhere
> in this repo. See §10 for the full legal posture.

---

## 0. Headlines

1. **One domain, three compiled-in drivers.** F2 (ZowieBox-class encoder/decoder management),
   the F1 display node's enrollment/management face, and F4 (Cast) are all the same shape: a
   registry entry + a driver actor + three projections. Building the vendor-neutral abstraction
   day one is justified — at least three peer vendors (Magewell Pro Convert, Kiloview, BirdDog)
   publish equivalent public HTTP control APIs, so the second driver is real and near-term.
2. **Invariant #10 by construction.** Drivers live in `multiview-control`, publish into
   watch/broadcast channels, and never touch the data plane. The proof shape is identical to the
   existing NMOS module (`crates/multiview-control/src/nmos/mod.rs`): model always compiled,
   sockets behind an off-by-default feature, nothing the engine awaits.
3. **No plugin system.** The driver set is a closed `#[non_exhaustive]` Rust enum
   (`zowietek`, `displaynode`, `cast`) with fixed probed capability flags. No capability
   descriptor registry, no `dyn Any`, no generic property bags. A new device family is a new
   enum variant + driver module + ADR.
4. **Honesty tiers are load-bearing.** Vendor decoders are **Tier C — bounded drift,
   ±100–500 ms** (no genlock/PTP/buffer knobs exist in the published API); Cast is **Tier D —
   none/seconds** (receiver buffers seconds; no sync surface at all). Sync groups compute and
   display the **weakest-member** tier and never over-claim.
5. **Discovery is new infrastructure.** No working mDNS code exists in the repo today — the NMOS
   transport (`crates/multiview-control/src/nmos/transport.rs`) is an explicitly compile-only
   seam. One shared mDNS/DNS-SD browse layer (an `mdns-sd`-class crate) is built once and used by
   all three drivers, and its results are **untrusted inventory requiring explicit confirm-adopt**
   ([ADR-0041](../decisions/ADR-0041.md) doctrine).

---

## 1. The mental model and the isolation proof

A *Device* is a control-plane registry entry for a network-managed box: a ZowieBox-class
encoder/decoder, one of our own display nodes, or a Cast target. It projects:

- **Source candidates** — when the device encodes, its streams appear in the Sources "Add" picker
  as "From device: …". Picking one creates a **normal managed Source** (`kind: rtsp|srt|ndi`)
  whose body carries a `device_ref` annotation. The engine ingest path is untouched: the
  framestore tile ladder, the input pacer, and supervised reconnect all apply as-is. There is
  deliberately **no** device feature on `multiview-input` — devices are a control-plane concern
  only.
- **Output targets** — when the device decodes, it appears in the Outputs "Add" picker as a
  destination. Picking one creates a **normal managed Output** with a `device_ref`; the driver
  configures the device's decode side to consume that output. Encode-once-mux-many (invariant #7)
  applies: pointing a device at an existing rendition is free fan-out (Class-1).
- **Display heads** — devices with physical scanout (display nodes; a ZowieBox HDMI-out counts)
  expose a Display tab: assign Program, a specific output/rendition, or a wall head from the
  existing `WallConfig`/`HeadConfig` model (`crates/multiview-config/src/wall.rs`).

**Why invariant #10 holds by construction.** Every driver is a poller/actor inside
`multiview-control` that (a) talks HTTP/TLS to its device on its own task or thread, (b) publishes
status into watch/broadcast channels consumed by the realtime layer, and (c) mutates only
control-plane stores. The engine never produces, consumes, or awaits anything device-shaped; a
hung device can at worst stall its own driver task. This is the same proof the NMOS and broadcast
router modules already carry (`crates/multiview-control/src/nmos/mod.rs`,
`crates/multiview-control/src/router/mod.rs`, `crates/multiview-control/src/routing.rs`), and the
CI chaos gate that enforces #10 covers the new channels the same way. Invariant #1 is untouched
for the stronger reason that no device code shares a process boundary with the tick loop's data
path at all.

---

## 2. Registry, record shape, and state machine

### 2.1 Store

Reuse the implemented generic resource store
(`crates/multiview-control/src/resource_store.rs`): opaque versioned `{id, name, body}` documents,
monotonic `Version` → `ETag`/`If-Match` → `412` (ADR-W006), one tested implementation
parameterised by a `ResourceKind` marker. Devices add `DeviceKind` and `SyncGroupKind` markers.
The store is in-memory and seeded from config — **config-as-code is the durable source** for
devices, exactly as for sources/outputs today (§7).

The `body` is a `multiview_config::Device` (new `device.rs` in `multiview-config`); runtime
status is a separate read-only projection (never in the body, never exported):

```jsonc
// GET /api/v1/devices/{id}/status   (primary delivery: conflated on Topic::Devices)
{
  "state": "ONLINE",            // DISCOVERED|ADOPTING|ONLINE|DEGRADED|AUTH_FAILED|UNREACHABLE
  "mode": "decoder",
  "capabilities": { "encode": false, "decode": true, "display": true,
                    "sync": "offset-only", "audio": true, "reboot": true,
                    "firmware_update": false },
  "streams": [ { "role": "decode", "output_ref": "out-program-srt",
                 "bitrate_bps": 5980000, "fps": 25.0, "healthy": true } ],
  "sync": { "group": "lobby-wall", "offset_ms": 120, "achieved": "bounded-skew" },
  "temperature_c": 47.5,
  "last_seen_ts": 920451123456
}
```

### 2.2 State machine

```
DISCOVERED ──adopt──▶ ADOPTING ──probe ok──▶ ONLINE ◀──recover── DEGRADED
                         │                     │  ▲                  ▲
                         │ bad creds           │  └─reconnect────────┘
                         ▼                     ▼
                    AUTH_FAILED           UNREACHABLE
```

- `DISCOVERED` — present only in the untrusted discovery inventory; not a registry entry.
- `ADOPTING` — record created, first probe in flight.
- `ONLINE` / `DEGRADED` — reachable; `DEGRADED` means a device-reported fault (e.g. decode
  stalled, over-temperature) while the management channel still answers.
- `AUTH_FAILED` — credentials rejected; the breaker opens immediately (no retry storm) and
  re-probe fires only when the secret is updated.
- `UNREACHABLE` — supervised reconnect exactly like inputs: exponential backoff + jitter +
  circuit breaker (same config shape as `Source.reconnect`), with a "Reconnect now" override.
  On reconnect the driver **re-converges desired state** (mode, decode binding, sync offset)
  and clears the alarm.

Offline beyond a dwell raises an X.733 alarm (`crates/multiview-core/src/alarm.rs` vocabulary)
on the existing Alarms surface, so webhook/email/SNMP routing comes free.

### 2.3 Driver model — the YAGNI guard

- **`driver` is a closed `#[non_exhaustive]` enum of compiled-in drivers:** `zowietek`,
  `displaynode`, `cast`. No plugin loading, no `dyn Any`, no reflection.
- **Capabilities are fixed probed flags:** `{encode, decode, display, sync, audio, reboot,
  firmware_update}` with `sync` a tri-state `frame-accurate | offset-only | none`. A driver maps
  its device into exactly the three projections plus this fixed status shape — we model "a
  network encoder/decoder", never any vendor's full feature tree. No device-side routing matrix,
  no device-side layouts, no per-device ABR.
- **Feature gating posture (the NMOS/router precedent):** config/model types and all handlers are
  always compiled and tested socket-free (`tower::oneshot`); driver network I/O sits behind
  per-driver off-by-default features (`zowietek`, `cast`, …) or the `devices-net` umbrella. The
  default `cargo check` stays pure-Rust, socket-free, CI-green. This is exactly how
  `nmos` (model always compiled, transport behind `#[cfg(feature = "nmos")]`) and the broadcast
  router (`#[cfg(feature = "router")]`) already work.

---

## 3. The `zowietek` driver (F2)

Targets ZowieTek's ZowieBox family of HDMI/SDI hardware encoder/decoders (budget class,
PoE-powered, NDI|HX-capable). Everything F2 needs exists in the vendor-published HTTP API:
three encoder channels (H.264/H.265/MJPEG) + audio encode, RTMP/SRT push, served RTSP/NDI, a
decode-URL table (RTSP/SRT/RTMP) + NDI|HX decode, HDMI output format/loop-out/mute, network/
WiFi/time/users, recording, snapshot, tally, reboot/standby, and CPU temperature.

### 3.1 Typed client — defensive by design

The device speaks JSON-RPC-over-HTTP-POST: a module path plus a query verb, with the operation
actually selected by `group`/`opt` fields in the JSON body, and a `{status, rsp, …}` response
envelope. The driver's typed client is built around hazards verified in the vendor doc:

| Hazard (verified) | Driver rule |
|---|---|
| Explicit "operation too fast" status codes; no numeric rate documented | **Serialize all requests per device**; poll each status group at ~≤1 Hz; back off on the rate-limit (`00009`) and the mid-reboot (`00010`) codes |
| Success code appears as both `"00000"` and `"000000"`; the human-readable `rsp` text drifts (`succeed`/`success`/variants) | Compare status codes leniently (numeric, not string-equal); never branch on `rsp` |
| The URL query verb is wrong on some documented calls (a set documented under the get verb, and vice versa) | Treat the query verb as **advisory**; the body `group`/`opt` are authoritative |
| LAN, mDNS, and port changes reboot the device **with no HTTP response** | Model those calls as fire-and-forget: send, expect the socket to drop, ride the UNREACHABLE→reconnect path, re-probe on return |
| Bitrate units conflict in the doc (parameter tables in kbps, examples in bps) | **Verified on hardware (2026-06-10)**: current firmware reports and accepts **bps** (e.g. `12000000` = 12 Mbps); the typed schema locks to a bps newtype with a magnitude sanity guard against kbps-shaped values |
| Login returns a `uuid` used only at logout; every documented URL carries a `login_check_flag=1` parameter; no session lifetime/expiry is documented | Implement the documented flow exactly (login → keep uuid → logout); treat session semantics as a security-relevant hardware verification item (the login/uuid flow and the five-zero `"00000"` success form are confirmed on current firmware); plain-HTTP credentials mean **recommend a management VLAN** in the docs |
| Some `getinfo` shapes return an **empty body** — no error JSON at all — when the group/opt does not fit the module or the current workmode (observed live) | Treat an empty body as a protocol error distinct from HTTP failure; never parse-or-default to success |

Status polling (poll-only — the device has no documented push eventing): stream/publish status,
the decode table, decoder state, HDMI input detection, CPU temperature, and storage status, each
at ~≤1 Hz per group.

### 3.2 Three facets (ADR-M009)

- **Source facet** — configure the device's video/audio encode channels; ingest via the device's
  served RTSP/NDI (the served URL paths are undocumented; on current firmware the RTSP mounts are
  verified as `rtsp://<device>:8554/main/av` + `/sub/av`, and the pull URL stays
  operator-overridable for other models/firmware)
  or have the device RTMP/SRT-push to a Multiview listener. Creates an ordinary managed Source
  carrying a `device_ref`.
- **Output facet** — manage the device's decode-URL table to point at a Multiview-served
  RTSP/SRT/RTMP or NDI output; HDMI output format/mute/loop-out; volume. Creates an ordinary
  managed Output with `device_ref`. Reusing an existing rendition is free fan-out (Class-1).
- **Management facet** — network/time/users, reboot/standby, tally lamp (usable as a cheap remote
  on-air indicator), temperature, recording to the device's storage, and NDI activation: NDI is
  **licence-gated per device** (some SKUs ship activated, others need a paid per-device key), so
  the driver surfaces activation state and accepts a key; decode is NDI|HX-family only — no
  full-bandwidth NDI.

### 3.3 Mode is a converged workmode state

The published API exposes **no encoder-vs-decoder mode endpoint**, but live firmware enforces a
real workmode state: in encoder workmode, decode-table calls reject with a "workmode" error
(status `00004`, observed 2026-06-10 on current firmware) — the modes are mutually exclusive
device states, not merely emergent appearance. The vendor-provided SDK is to be checked for a
workmode-switch call during implementation; the documented route remains: a decode URL (or NDI
receive) is enabled and the HDMI output shows it; encode channels are always present.
The driver therefore models a `desired_mode` and **converges** it: the device enforces
close-before-open semantics (dedicated "close it before operating" status codes), so convergence
is stop-current → start-next, with the impact declared **before** apply per the instant-apply
doctrine: *"device restarts its pipeline; bound sources ride the tile ladder to NO_SIGNAL; no
Multiview outputs are affected."* Decode switching on the device has a visible gap on that
device's display — there is no device-side make-before-break, and we say so in the UI.

### 3.4 IPv4 island handling

The vendor API is IPv4-only end-to-end (zero IPv6 occurrences in the published doc). The driver
accepts hostnames and IPv6 literals per repo policy and treats IPv4-only devices as a **legacy
device limitation** under [conventions §10](../architecture/conventions.md)'s legacy-interop
allowance ([ADR-0042](../decisions/ADR-0042.md)); all Multiview-side surfaces stay IPv6-first.

### 3.5 What we will NOT do

- **No vendoring or redistribution** of the vendor API documentation, UI concepts, screenshots,
  logos, or trade dress — original endpoint docs only, written from declared/observed behaviour.
- **No modelling of the vendor's full feature tree** — only the three projections + the fixed
  status shape (also the vendor-neutrality guard).
- **No firmware upgrades through our API.** The published API has no firmware-version, model,
  serial, uptime, log-retrieval, or upgrade endpoint (upgrade is web-UI-only); fleet inventory is
  partial and upgrades stay manual. We surface "update available" guidance only.
- **No reverse-engineering of the undocumented WebSocket port** for v1 — poll-only, with a
  packet-capture investigation noted as a possible later improvement.

### 3.6 Tier C and limits, stated plainly

**Sync tier: C — bounded drift, ±100–500 ms box-to-box.** Verified: no genlock, PTP, reference
input, decoder-buffer, or latency control exists anywhere in the public API or manuals; the
decoder free-runs (vendor release notes even record a fixed cumulative-delay-during-decode bug).
SRT/TSBPD feeds are recommended to bound *delivery* skew; an optional **scheduled re-sync**
(stop/start decode at an aligned instant) re-zeroes drift but blanks that device's output ~1–3 s —
opt-in only (e.g. nightly). If hardware probing shows the undocumented decode-buffer field is a
usable knob, the tier may improve to ±50–150 ms — **measured, never promised**.

Other hard limits: poll-only with an undocumented rate limit; one NDI decode at a time and an
unstated decode-table capacity; plain HTTP with murky session semantics; served stream URL paths
undocumented (operator-supplied or probed); doc-vs-firmware drift (doc v1.5 vs current 2.0.x
firmware) means the typed client must be validated on real hardware before its schema is trusted.

---

## 4. The `displaynode` driver (F1's management face)

The display node itself — `multiview node`, one supervised ingest → hardware decode → KMS atomic
scanout — is specified in [display-out](display-out.md) and
[ADR-0045](../decisions/ADR-0045.md). This driver is its control-plane identity:

- **Enrollment (zero-touch):** the operator mints a TTL'd enrollment token in Settings (one-time
  display, hashed at rest — the existing API-token pattern). A node image baked with the
  controller URL + token enrolls itself on boot (`POST /api/v1/devices/enroll` with token +
  public key + model + EDID-derived head list) and appears in Devices already ONLINE.
- **Pairing (no baked token):** the node boots to a pairing card on its attached display — a QR
  code **plus** a 6-character code in large type (both forms; never QR-only, per WCAG). The
  operator completes pairing in the WebUI (`POST /api/v1/devices/pair`), which binds the device
  record to that node's keypair.
- **Keypair identity:** display nodes authenticate by their enrolled keypair thereafter — no
  password, nothing to rotate in a secret store.
- **Assignment:** the device's Display tab assigns **Program** (default), a specific
  output/rendition, or a **wall head** from `WallConfig`/`HeadConfig`
  (`crates/multiview-config/src/wall.rs`). Reassignment is node-local make-before-break: the node
  starts the new stream, switches scanout at a frame boundary, then drops the old one — Class-1
  from the engine's point of view; the program output never blips.
- **Sync:** display nodes are the `sync: "frame-accurate"` members (Tier A/B); the presentation
  discipline (epoch + link offset + vblank frame chooser) is display-out's §sync and
  [ADR-M010](../decisions/ADR-M010.md). A node that loses the controller keeps its last epoch and
  free-runs, drift-bounded; on rejoin it re-converges its offset **before unmuting scanout**.

---

## 5. The `cast` driver (F4, stretch)

A thin control-plane sink: discover via mDNS `_googlecast._tcp`, connect TLS to the advertised
port (8009 for devices; Cast *groups* advertise other ports — always use the advertised one),
LAUNCH the Default Media Receiver, LOAD an existing Multiview HLS rendition URL. The device
fetches media itself; the engine and encoders are untouched (encode-once preserved; invariants
#1/#10 trivially safe).

**The verified verdict, stated plainly: server-initiated casting of the program stream requires
NO public URL, NO Google developer registration, and NO cloud account.** The Default Media
Receiver (app ID `CC1AD845`) is Google-hosted and registration-free; only the **media URL** must
be reachable **by the device**, and plain-HTTP LAN URLs are fine (media is exempt from
mixed-content rules). This is exactly the long-established Home Assistant / pychromecast pattern.

### 5.1 Session actor lifecycle

One actor per device session (blocking thread or task — the candidate client crate is
synchronous), pure control plane:

- TLS connect (device presents a self-signed cert; verification is disabled by design, as the
  entire sender ecosystem does) → virtual CONNECT → LAUNCH `CC1AD845` → CONNECT to the app's
  transport → LOAD `{contentId: <HLS URL>, streamType: LIVE}` with the fMP4/CMAF segment-format
  hint (TS is the receiver's default assumption).
- **Heartbeat: PING every 10 s; the session is dead after 20 s without a PONG. Reconnect retries
  at 5 s intervals**, re-resolving by mDNS UUID first (DHCP may have moved the IP). On reconnect,
  GET_STATUS; if the session is gone, re-LAUNCH + re-LOAD.
- The receiver platform kills **IDLE** apps after ~5 min (the no-idle-timeout flag exists only
  for custom receivers); an actively-PLAYING LIVE stream is not killed. The supervisor watches
  media status for IDLE + idle-reason and re-LOADs.
- Sessions are **ephemeral by default** (runtime-only, never exported); "Save as device" promotes
  one to a persisted device record keyed by the mDNS UUID.

### 5.2 Prerequisites and hard realities

1. **CORS on the HLS endpoints** — the receiver is a browser app on a Google origin and fetches
   playlists/segments cross-origin; CORS headers are currently absent from our HLS server
   (verified by search) and are a standalone prerequisite slice that benefits all browser
   consumers.
2. **IP-literal (or publicly resolvable) media URLs** — Cast devices **ignore DHCP-provided DNS
   and resolve via Google's public DNS**, so `.local` and LAN-DNS names do not resolve on the
   device. We hand the device an IP-literal URL.
3. **Legacy-IPv4 interop** — Cast is effectively IPv4-only in practice (devices tolerate IPv6
   poorly; treat AAAA as untested). The media URL we hand over is the one place we deliberately
   produce an IPv4/dual-stack form, documented under conventions §10's legacy-interop allowance —
   never as a counterexample to IPv6-first.
4. **Codec floor: H.264 High@≤4.1 + AAC** for universal device coverage (the 1st/2nd-generation
   ceiling); HEVC requires fMP4 — our CMAF-first HLS already complies.
5. **LL-HLS does not engage.** Although the receiver's HLS engine switched to Shaka Player
   (production default flipped ~April–May 2026), it ships with low-latency mode off and the
   Default Media Receiver exposes no configuration hook. Serve standard-latency HLS; expect
   **6–30 s glass-to-glass** — Tier D, never part of a synchronized canvas.
6. **Custom/styled receivers are operator-opt-in only and out of scope:** branded UI,
   receiver-side overlays, or disabling the idle timeout require the Cast developer console
   (one-time US$5 fee) and public valid-CA HTTPS hosting — both excluded by the no-paid-services
   default; unnecessary for "play the composited program on a TV".

### 5.3 What we will NOT do

- **No sync engineering** (Tier D); no video-group concept exists in Cast at all.
- **No Cast SDK, no developer-console registration, no "Works with Google Cast" badge or implied
  certification.** Nominative use only, with a trademark attribution line (§10).
- **Best-effort transport, stated in the ADR:** the wire protocol is proprietary; the defensible
  implementation path is Google's own BSD-3-Clause Open Screen sources plus community
  documentation, and the (large, old, unchallenged) ecosystem precedent. Google could change the
  protocol at any time — this is a convenience output, never a guaranteed transport.
- Failure modes designed in: device sleep / IP change (re-resolve by UUID), receiver hijack by
  another sender (surface "preempted"), cross-VLAN mDNS invisibility (manual `address:port`
  entry escape hatch), firmware drift (hardware validation pinned to current firmware across
  ≥2 device generations).

---

## 6. Discovery — new shared infrastructure

No working mDNS code exists in the repo (the NMOS transport is a compile-only seam, verified);
discovery is **new** infrastructure built once and shared:

- One mDNS/DNS-SD browse layer (an `mdns-sd`-class crate, MIT/Apache) behind the discovery
  feature gate, serving `zowietek` browse (service type undocumented by the vendor — empirical
  verification item; hostname `.local` resolution at minimum), `_googlecast._tcp` for Cast, and
  `_multiview-ctl._tcp` for display-node → controller resolution. The zowietek type is an
  **operator-configured knob** (as built: the config document's `[discovery]` section —
  `zowietek_service_type`, plus `extra_service_types` for additional browse types); it is never
  fabricated from a built-in constant, and an unconfigured deployment reports such services
  `unknown`.
- Results are an **untrusted inventory requiring explicit confirm-adopt** — the
  [ADR-0041](../decisions/ADR-0041.md) doctrine (operator confirm-binds; never auto-ingest)
  applied to devices.
- IPv6-first presentation: AAAA addresses lead, IPv4 entries are explicitly labelled *legacy*.
- Scans are **bounded and rate-limited** (ADR-M008): single-flight (one in-flight browse —
  concurrent `mdns-sd` browses of one type corrupt each other's listeners/queriers; a concurrent
  scan request attaches to the running scan's operation id), time-budgeted, capped in collected
  events, and every responder-controlled string/list is truncated before retention.

---

## 7. API resources, events, and config-as-code

### 7.1 API resources (all under `/api/v1`, OpenAPI-first via `cargo xtask gen-openapi`)

```
GET/POST           /devices                      # CRUD; Idempotency-Key; ETag/If-Match
GET/PUT/DELETE     /devices/{id}                 # DELETE 409s if sources/outputs still bound
GET                /devices/{id}/status          # RO snapshot (primary delivery = realtime)
POST               /devices/{id}/probe           # 200
POST               /devices/{id}/set-mode        # 202 + operation id; declared impact
POST               /devices/{id}/reboot          # 202
POST               /devices/{id}/identify        # 204 (blink LED / flash card)
POST               /devices/{id}/test-pattern    # 204 (sync-capable)
GET                /devices/{id}/source-candidates
GET                /devices/{id}/output-targets
POST               /discovery/devices/scan       # 202; GET /discovery/devices = untrusted inventory
POST               /devices/enroll               # node→controller (TTL'd token, keypair-bound)
POST               /devices/pair                 # operator completes screen pairing (6-char code + QR)
GET/POST           /sync-groups ; GET/PUT/DELETE /sync-groups/{id} ; POST /sync-groups/{id}/measure
```

Action style: **bare verb segments**, matching the shipped routes `/api/v1/salvos/{id}/arm` and
`/api/v1/alarms/{id}/ack` (`crates/multiview-control/src/routes/mod.rs`); codified in
[ADR-W017](../decisions/ADR-W017.md) since the capability-matrix brief sketches a `:verb` style
the code never adopted. Long-running actions return `202 Accepted` + an operation id with the
result on the realtime stream (ADR-W008); errors are RFC 9457 `application/problem+json`;
`Idempotency-Key` on creates and actions; `ETag`/`If-Match` → `412` everywhere. Examples lead
IPv6 with bracketed literals.

### 7.2 Events ([ADR-RT007](../decisions/ADR-RT007.md))

One coarse `Topic::Devices` added to the closed `Topic` enum
(`crates/multiview-events/src/topic.rs`), fine-grained via the existing `ids` filter (the
realtime brief's rule: coarse topics + ids filtering, not more topics). Event types:

- `device.status` — **conflated, latest-wins** (state/mode/streams/temperature), like
  `output.bitrate`;
- `device.adopted` / `.removed` / `.mode` / `.error` / `.sync` — low-rate, lossless lifecycle;
- `device.discovered` — browse results streamed while a scan operation runs;
- `TimingStatus` — the F3 epoch/skew telemetry rides the same topic (shared with
  [display-out](display-out.md) / [ADR-M010](../decisions/ADR-M010.md)).

All events originate from driver poller tasks' watch/broadcast channels in the control plane —
the engine never produces or awaits them (§1).

### 7.3 Config-as-code

Declarative adoption is the config; runtime state is not. The export carries **desired state
only** — address, credentials ref, desired mode, bindings, sync groups. Firmware, temperature,
online state, current streams, achieved skew, discovered-but-unadopted devices, and ad-hoc Cast
sessions are never exported. Applying a config performs idempotent adoption + convergence on boot
(a GitOps-able node fleet).

```toml
[[devices]]
id = "dev-foyer-decoder"
display_name = "Foyer decoder"
driver = "zowietek"
address = "http://[fd00:db8::42]"        # IPv6-first; device itself may be IPv4-legacy
desired_mode = "decoder"
alarm_on_offline = "major"
[devices.auth]
secret_ref = "op://Site/foyer-decoder/credentials"   # write-only secret store; export = ref|redact
[devices.reconnect]
initial_ms = 500
max_ms = 30000

[[devices]]
id = "dev-node-left"
driver = "displaynode"                   # enrolled keypair identity, no password
[devices.display]
assign = { wall_head = "head-l" }        # or { program = true } / { output = "out-…" }

[[sync_groups]]
id = "lobby-wall"
mode = "auto"                            # auto = weakest-member tier
target_skew_ms = 50
members = [
  { device = "dev-node-left",     offset_ms = 0 },
  { device = "dev-node-right",    offset_ms = 0 },
  { device = "dev-foyer-decoder", offset_ms = 120 },
]
```

Secrets reuse the existing model wholesale: `secret_ref` strings (already in
`crates/multiview-config/src/schema.rs`), the write-only secret store, export offering
`secrets=ref|redact` only. Enrollment/pairing tokens are minted server-side, shown once, stored
hashed; display nodes thereafter authenticate by keypair.

---

## 8. Sync groups

A first-class versioned resource (`/sync-groups`), specified jointly with
[ADR-M010](../decisions/ADR-M010.md):

- **Achieved tier = weakest member**, computed and displayed immediately, never over-claimed:
  "bounded-skew — limited by dev-foyer-decoder (offset-only)" with per-member badges. This
  encodes the wall-clock-sync honesty rule ([wall-clock-sync](wall-clock-sync.md)): badge what is
  *achievable*, never "synced" generically.
- **Per-member `offset_ms`** — the AES67-link-offset semantics applied to video: uniformity
  across members is what matters, not smallness. Applying an offset change is Class-1: nodes trim
  their presentation buffers at a frame boundary; the engine's output cadence is untouched (it is
  the house clock).
- **Actions:** `test-pattern` (burnt-in frame counter + flash on all members so the operator can
  eyeball/photograph skew) and `measure` (202 job; driver-assisted estimates where a device
  reports anything useful).
- **Drift alarms:** a member drifting beyond `target_skew_ms` + dwell raises a `degraded-sync`
  warning on the existing Alarms surface.

### The published tier table

Tiers S/A/B are display-out territory (our own nodes); they are listed here for the complete
operator-facing picture. **Tiers C and D are hard-capped by the devices themselves** — no
controllable knobs exist — and the UI badges say so:

| Tier | Output class | Achieved |
|---|---|---|
| **S** | Same node, multi-head | Frame-accurate; sub-ms flip delta typical (single atomic commit); CRTCs still free-run |
| **A** | Display nodes + PTP | Frame-accurate (same frame index); +0–1 refresh vsync-phase residual |
| **B** | Display nodes + chrony | Frame-accurate; occasional ±1-frame decisions at boundaries |
| **C** | ZowieBox-class decoders | **Bounded drift ±100–500 ms**; optional scheduled re-align (blanks that device 1–3 s) |
| **D** | Cast | **None / seconds**; never part of a synchronized canvas |

There is **no genlock-grade scanout-phase alignment anywhere in this domain** — that needs
framelock hardware and is an explicit non-goal. Multi-node *audible audio* defaults to a
single-audio-node policy (two free-running rooms of audio = comb filtering/echo); never two
vendor decoders audible in one room.

---

## 9. SPA surface

- New nav entry **Devices** (between Outputs and Monitoring, in `web/src/app/navigation.tsx`).
- A new `DevicesPage` list (TanStack Table, like the existing pages): state badge (never
  colour-alone — WCAG), mode, firmware chip where known, temperature, current stream link,
  sync-group chip, last-seen. Fed by the `devices` topic snapshot+delta, degrading gracefully to
  REST polling.
- Device detail tabs: **Overview / Display / Streams / Sync / Maintenance / Events**. Maintenance
  actions (reboot, identify) are 202 jobs via the existing `submitOperation` helper
  (`web/src/api/operations.ts`); alarms surface on the existing `web/src/pages/AlarmsPage.tsx`.
- A Sync Groups page; **"From device"** sections in the Sources/Outputs pickers fed by the
  projection endpoints — the layout editor's insertion point is
  `web/src/layout/components/SourcePalette.tsx`; data access follows the resources pattern
  (typed field guards, React Query + ETag maps, `web/src/resources/queries.ts`), and devices land
  **in the OpenAPI spec first** (`cargo xtask gen-openapi`).
- Settings → Display Nodes: node image/package download + enrollment tokens (one-time display,
  hashed at rest).
- In-app help (nav in `web/src/pages/docs/docsNav.tsx`): `/help/devices`, `/help/devices/adopt`,
  `/help/display-nodes`, `/help/sync`, `/help/casting`, plus glossary and config-reference
  updates.

---

## 10. Capability-matrix classification

New rows for [management-capability-matrix](management-capability-matrix.md) (legend: CP =
control-plane only; C1 = Class-1 hot; C2 = Class-2 make-before-break; **DEV = device-side reset,
program output unaffected** — a new legend entry this domain introduces):

| Operation | Class | Notes |
|---|---|---|
| Adopt / remove / edit device record | CP | Registry only |
| Probe / status / identify / test-pattern | CP | |
| Bind device stream as Source | C1 | Additive input; normal tile ladder |
| Send program to device (existing rendition) | C1 | Free fan-out; "shares encoder" badge |
| Send program to device (new rendition) | C1-additive | Admission-gated (409 if budget exceeded); placement re-assess per instant-apply doctrine |
| Display-node content/layout reassignment | C1 | Node-local make-before-break; engine untouched |
| Sync-group offset change | C1 | Node buffer trim at frame boundary; engine cadence untouched |
| Local display head modeset (resolution/refresh) | C2 | Modeset blanks that head briefly; program output unaffected |
| Device mode convergence (encoder⇄decoder) | DEV | Device pipeline restarts; declared impact pre-apply |
| Device LAN/mDNS/port change; reboot; firmware | DEV | Device reboots (some vendor sets return no HTTP response) |
| Cast session start/stop | CP + C1 | Free fan-out of an existing HLS output |
| Scheduled vendor-decoder re-sync | DEV (opt-in) | Blanks that device 1–3 s at the scheduled instant |

---

## 11. Failure UX

| Failure | Operator sees | Auto-recovery |
|---|---|---|
| Device unreachable | `UNREACHABLE` + last-seen; X.733 alarm after dwell; bound Sources ride the normal tile ladder; Outputs ride their failover policy | Supervised reconnect exactly like inputs (backoff + jitter + breaker; "Reconnect now" overrides a parked breaker); on reconnect the driver re-converges desired state and clears the alarm |
| Wrong credentials | `AUTH_FAILED` (distinct from UNREACHABLE); field-level problem on the credentials control; warning alarm | No blind retries — the breaker opens immediately; re-probe fires when the secret is updated |
| Device-side decode stall | `DEGRADED`; stream row "decoding stalled (device-reported)"; warning alarm noting **program output is unaffected** | Bounded re-kick with backoff; escalate + one-click reboot; never auto-reboot mid-show without the opt-in policy |
| Display-node loss mid-show | Same as unreachable; the node itself holds last-good frame then its local slate (the node inherits the product's resilience doctrine) | Node reconnects with its enrolled identity, re-pulls, and re-converges its sync offset **before unmuting scanout** |
| Cast session drops | Toast + chip clears; info-severity event (ad-hoc sessions never raise major alarms) | One retry, then stop; ephemeral session GC |

Throughout: **program output continuity is never contingent on any device** (invariants #1/#10),
and the UI says so in the alarm detail. The failure surfaces are the existing Alarms +
per-resource status patterns — no new notification system.

---

## 12. Legal and trademark posture

- **Clean-room interop, no doc redistribution.** The driver consumes a vendor-published HTTP API
  whose stated purpose is third-party automation, on customer-owned LAN devices, never a vendor
  cloud — the vendor itself directs customers to third-party controllers, and an MIT-licensed
  third-party controller module for this exact API ships publicly without objection. The concrete
  copyright exposure is **redistributing the documentation text**, so: never commit the vendor
  PDF/text; all endpoint descriptions in this repo are original re-expressions; independently
  written client code issuing documented HTTP requests is not a derivative work under any
  mainstream doctrine.
- **Nominative trademark use only.** "Works with ZowieTek ZowieBox-series devices", plain text,
  no logos/trade dress, with a non-affiliation disclaimer: *"ZowieTek, ZowieBox and related marks
  are trademarks of Zowietek Electronics, Ltd. This project is independent and not affiliated
  with or endorsed by Zowietek."* Same pattern for Google: *"Google Cast is a trademark of Google
  LLC"*; **never** the "Works with Google Cast" badge or any implied certification.
- **Residual risk, accepted and mitigated.** The vendor's API terms are revocable and PRC-law
  governed and permit unilateral modification — practically unenforceable against end users
  controlling their own LAN hardware, but it argues for exactly what we build: a pluggable,
  removable driver inside a vendor-neutral abstraction. Further mitigated: the vendor has
  **provided their SDK to this project with permission to integrate** (2026-06-10), conditioned
  only on non-endorsement clarity — "Supports ZowieBox" phrasing is acceptable; "Official
  ZowieBox" or any endorsement implication is not. The Cast wire
  protocol is proprietary; implementing from Google's own open-sourced BSD-3-Clause Open Screen
  code plus community documentation is the defensible path, with a large unchallenged ecosystem
  precedent (pychromecast / Home Assistant / VLC).
- Both postures fold into the already-pending counsel review; proceed-and-review is the default.

---

## 13. Hardware validation (gates the claims, not the build)

| Target | Validates |
|---|---|
| ZowieBox units (×2, NDI-licensed, available in the test environment) | Remaining: session semantics, rate limits, decode-table capacity, control-API mDNS service type, workmode-switch via the vendor SDK, box-to-box skew histogram for the Tier-C ±100–500 ms bound (the decode-buffer field `vo.bufnum` is confirmed present on current firmware). Already verified live (2026-06-10): bps bitrate units, served RTSP paths, login/status semantics, `_ndi._tcp` advertisement in encoder workmode |
| The HP t630 test unit (approved as a dedicated display-node target) / Raspberry Pi 4 (being provisioned; Pi 5 still to acquire) | Display-node enrollment/pairing, assignment, sync acceptance soak — per [display-out](display-out.md) |
| Cast devices (a multi-generation fleet is available in the test environment: Chromecast Ultra primary target, Google TV-class, Nest Hub-class, built-in TV implementations, audio groups on non-default ports) | Session actor, HLS playback with CORS, real glass-to-glass latency, firmware-pinned behaviour, multi-hour LIVE soak (some smart-display models reportedly kill long LIVE sessions early); a CASTV2-over-IPv6 probe (fleet devices advertise AAAA mDNS records) |
| The GPU test server | Engine-side regressions: encode fan-out for device-bound outputs, invariant-#10 chaos gates over the new device channels |

---

## 14. External references

- *ZowieTek API documentation v1.5* (2025, vendor-published) — the device API surface the
  `zowietek` driver targets; not redistributed in this repo.
- Vendor-published ZowieBox product/firmware pages and knowledge base (encode/decode/NDI|HX
  capabilities, NDI licence keys, firmware channel).
- Chromium **Open Screen** sources (BSD-3-Clause) — the authoritative open Cast protocol
  reference (cast channel protobuf + namespace JSON schemas).
- Google Cast developer documentation — Default Media Receiver, supported media/CORS, codec
  matrix per device generation.
- `rust_cast` crate (MIT) and `mdns-sd`-class discovery crates — candidate dependencies, licence
  chain clean for `cargo deny`.
- Peer vendor public control APIs (Magewell Pro Convert, Kiloview, BirdDog) — the evidence base
  for building the vendor-neutral abstraction day one.

## Decision records

- [ADR-M008 — Managed-device registry and compiled-in driver model](../decisions/ADR-M008.md)
- [ADR-M009 — Device stream binding (source-candidate / output-target / display-head projections)](../decisions/ADR-M009.md)
- [ADR-M010 — Multi-output timing & sync (outbound epoch, link offset, sync groups, published tiers)](../decisions/ADR-M010.md)
- [ADR-M011 — Cast output driver (protocol stance, legal framing, ephemeral sessions, legacy-IPv4 interop)](../decisions/ADR-M011.md)
- [ADR-RT007 — Devices realtime topic and event types](../decisions/ADR-RT007.md)
- [ADR-W017 — Action route style: bare verb segments](../decisions/ADR-W017.md)

Related, owned by the sibling brief [display-out](display-out.md):
[ADR-0044](../decisions/ADR-0044.md) (DRM/KMS display sink) and
[ADR-0045](../decisions/ADR-0045.md) (display-node mode, enrollment/pairing surface).
