# ADR-T014: WHIP ingest — WebRTC contribution sources (RFC 9725)

- **Status:** Proposed
- **Area:** Streaming/Timing (ingest)
- **Date:** 2026-06-10
- **Source brief:** [webrtc.md](../research/webrtc.md) (WHIP ingest + audio de-embed
  sections; ships in the same push)
- **Relates to:** [ADR-0048](ADR-0048.md) (the shared WebRTC transport endpoint —
  `multiview-webrtc` crate, single-socket mux, full ICE, session GC),
  [ADR-0049](ADR-0049.md) (WebRTC program outputs), [ADR-P006](ADR-P006.md) (WHEP
  preview completion), [ADR-W023](ADR-W023.md) (SPA player + forms),
  [ADR-T002](ADR-T002.md) (last-good-frame + tile state machine),
  [ADR-T003](ADR-T003.md) (PTS normalization — `WrapBits::Rtp32`),
  [ADR-T013](ADR-T013.md) (the shared RTP-audio → `AudioStore` rebase seam),
  [ADR-E001](ADR-E001.md) (decode-at-display-resolution, invariant #6),
  [ADR-R004](ADR-R004.md) (live-apply classification, invariant #11),
  [ADR-R005](ADR-R005.md) (program bus / canonical 48 kHz f32 audio),
  [ADR-0042](ADR-0042.md) (IPv6-first), invariants #1 (output-clock),
  #2 (last-good-frame), #3 (unified timing), #5 (NV12), #6
  (decode-at-display-resolution), #10 (isolation)

## Context

Browsers and contribution encoders (OBS Studio ≥ 30, GStreamer `whipclientsink`,
hardware encoders) publish low-latency WebRTC streams via **WHIP — the WebRTC-HTTP
Ingestion Protocol, RFC 9725**: one HTTP `POST` of an SDP offer, one `201` answer,
then ICE/DTLS/SRTP media to the server. Multiview has the *pure* half of a WebRTC
receive path already built and tested in
`crates/multiview-input/src/webrtc/`: the negotiated-media model (`sdp.rs` —
H.264/Opus codecs, payload types, directions) and payload-type router (`route.rs`);
SDP offer/answer negotiation itself is str0m's (see §10), not hand-rolled here. Plus
the application-layer **`MediaEngine`** seam
(`transport.rs` — the engine pulls decrypted RTP; the crate never links a
socket/crypto stack), and the bounded, keyframe-gated **`H264Depacketizer`**
(RFC 6184 single-NAL/STAP-A/FU-A, `MAX_REORDER_PACKETS = 128`,
`MAX_ACCESS_UNIT_BYTES = 8 MiB`, drop-never-grow). What is missing is everything
around that core:

- **No HTTP endpoint.** There is no WHIP resource in `multiview-control`, no auth,
  no session URL, and no configured source kind that derives one.
- **No native transport.** Nothing drives `MediaEngine` — [ADR-0048](ADR-0048.md)
  ships the shared str0m endpoint (one dual-stack UDP socket bound `[::]`, full-ICE,
  IPv6-first candidates) that WHIP ingest sessions mux onto alongside WHEP egress.
- **No decode.** `WebRtcProducer::to_produced` (`transport.rs`) currently stamps the
  reassembled **NAL bytes** as if they were NV12 pixels, with geometry taken from
  declared constructor arguments — a scaffold dishonesty that must be fixed by a
  real decoder.
- **No audio.** `WebRtcProducer::next_frame` drops every non-video payload type;
  the Opus PT has nowhere to go ([ADR-T013](ADR-T013.md) pins the rebase seam it
  must route through).

A WHIP source also inverts the usual ingest direction: Multiview is the **server**.
There is no URL to dial and re-dial — a configured source waits for a publisher,
and "reconnect" means accepting the next session. The endpoint semantics, auth,
lifecycle, and apply classification therefore need pinning once, before the wiring
lands.

## Decision

`kind = "webrtc"` becomes a first-class configured source: an RFC 9725 WHIP
endpoint derived from the source id, terminated by the shared
[ADR-0048](ADR-0048.md) transport, decoded through a new packet-fed
`multiview-ffmpeg` H.264 decoder into the standard `TileStore`/`AudioStore`
pipeline. Eight parts:

### 1. Source kind + derived endpoint URL

`multiview-config::schema::SourceKind` gains (internally tagged, house pattern —
never `untagged`):

```toml
[[sources]]
id = "cam-field-1"
kind = "webrtc"
token = "s3cret…"   # optional per-source bearer (RFC 6750)
audio = true        # default true; false answers the audio m-line inactive
```

The WHIP endpoint URL is **derived, never configured**:
`POST /api/v1/whip/{source_id}` (e.g.
`https://[2001:db8::10]:8443/api/v1/whip/cam-field-1`); the session resource is
`/api/v1/whip/{source_id}/sessions/{session_id}`. One publisher per source at a
time. The SPA source form displays the derived URL + token with a copy button
([ADR-W023](ADR-W023.md)). The plaintext `token` follows the existing
config-secret posture (rtmp/srt URLs already embed stream keys in config): it is
returned to authorized readers, present in config export, and migrates together
with url-embedded keys if/when a `secret_ref` indirection lands.

### 2. HTTP semantics (RFC 9725, exact)

All errors are RFC 9457 `application/problem+json` (house API convention,
[conventions.md §6](../architecture/conventions.md)). Routes registered in OpenAPI
(utoipa triple registration + the route-completeness assertion:
`ApiDoc::rest_routes` in `crates/multiview-control/src/openapi.rs` vs the mounted
router, tested in `tests/openapi.rs`). `application/sdp` request bodies are capped
at **64 KiB** (an explicit axum body limit on these routes).

| Request | Response |
|---|---|
| `OPTIONS /whip/{source_id}` | CORS preflight; `Accept-Post: application/sdp`. No `Link` ICE-server headers — Multiview does not hand clients STUN/TURN servers to use; its own candidates (host + advertised, plus relayed via the in-crate TURN client where configured — [ADR-0048 §5.1](ADR-0048.md), Amendment 2026-06-13) are gathered server-side and returned in the answer. |
| `POST /whip/{source_id}` (`application/sdp` offer) | **`201 Created`** + `Location: …/sessions/{session_id}` + `application/sdp` answer body. No `ETag`: RFC 9725 ties the entity-tag to ICE-restart support, which we do not implement — advertising it would be a lie. |
| `POST` with any other content type | **`415 Unsupported Media Type`**. |
| `POST` with a malformed offer | **`400 Bad Request`**. |
| `POST` with an offer sharing no supported codec (`WebRtcError::NoCompatibleCodec` — we answer **H.264 + Opus only**) | **`406 Not Acceptable`** (ecosystem practice, no RFC attribution; the existing preview WHEP routes keep their shipped mapping). |
| `POST` with no/invalid credentials | **`401 Unauthorized`** + `WWW-Authenticate: Bearer`. |
| `POST` with valid credentials lacking rights (e.g. a View-scope API key) | **`403 Forbidden`**. |
| `POST` while another publisher session is live on this source | **`409 Conflict`** — first publisher wins; the slot frees on `DELETE` or idle GC. |
| `POST` when the endpoint cannot admit the session (resource exhaustion — WHIP ingest sessions are admitted **outside** the `webrtc.max_sessions` viewer pool and are bounded by the count of configured `webrtc` sources, so a viewer flood never starves a publisher, ADR-0048) | **`503 Service Unavailable`** + `Retry-After`. |
| `PATCH …/sessions/{id}` | **`405 Method Not Allowed`** + `Allow: DELETE, OPTIONS` — honest, not a stub: we run **vanilla ICE** (the endpoint gathers all candidates before answering) and support neither trickle ICE nor ICE restart. RFC 9725 makes both **OPTIONAL** for both ends and forbids server→client trickle outright; its only `PATCH`-rejection code (`422`) covers the supports-one-kind case, and the supports-neither case is unspecified by the RFC — so the rejection is RFC 9110 generic method semantics (405 + `Allow`), matching deployed-server practice. Host-only gathering is near-instant, so trickle buys nothing here. |
| `DELETE …/sessions/{id}` | **`200 OK`**, idempotent — a repeat `DELETE` inside the 60 s session-tombstone window (ADR-0048) is still `200`; a never-known session id is `404`. |

**Auth:** the per-source `token` as a `Bearer` (RFC 6750), **or** a control-plane
API key with Write scope. A source with `token = None` accepts **only** Write-scope
API keys — a publish endpoint is never anonymous. Session ids are **≥128-bit
random** (`rand` from the OS RNG, hex/base64url-encoded), never sequential
(ADR-0048), and `DELETE …/sessions/{id}` requires the **same credential class** as
the `POST` that created the session — the session URL alone is not a capability.
The media-signalling CORS layer (`webrtc.cors_allow_origins`, default `*`,
ADR-0048) exposes `Location`/`Link` (`Access-Control-Expose-Headers: location,
link`) so browser publishers can read the session URL cross-origin
([ADR-W023](ADR-W023.md)).

### 3. Control stays native-free — the `WhipProvider` seam

`multiview-control` never links str0m. A **`WhipProvider`** trait (the mirror of
`WhepProvider`, `crates/multiview-control/src/preview.rs`) carries the negotiation:
`negotiate(source_id, offer, bearer) → Result<WhipAnswer, WhipReject>` +
`release(source_id, session_id) → bool`, both synchronous, never awaiting the data
plane. `WhipReject` variants map onto the 400/401/403/406/409/503 rows above; 415/405
are route-level and never reach the provider. The default `NoWhip` provider (the
pure / negotiation-only build) answers every offer `503` — routes stay present and
authz-enforced, exactly like `NoWhep`. `multiview-cli` implements the provider over
`multiview-webrtc` ([ADR-0048](ADR-0048.md)).

### 4. Video media path

The session runs an **RTP-mode** `Rtc`: str0m decrypts SRTP and surfaces RTP
packets, which cross a bounded drop-oldest ring (ADR-0048) into the ingest thread's
`MediaEngine` implementation — the existing pure seam is the canonical
depacketization path, **not** str0m's sample API (the keyframe-gated, bounded
`H264Depacketizer` is already property-tested; bypassing it for a second
depacketizer would fork the contract). From there:

```
MediaEngine::poll_rtp → H264Depacketizer (keyframe gate, RFC 6184)
  → multiview-ffmpeg packet decoder (NEW, `ffmpeg`-gated)
  → NV12 + SPS geometry + VUI ColorInfo
  → IngestPump (PtsNormalizer, WrapBits::Rtp32) → TileStore
```

- **The packet decoder is new in `multiview-ffmpeg`:** an avcodec-only (no
  `AVFormatContext`) H.264 decoder fed reassembled access units —
  start-code-framed Annex-B, with SDP `sprop-parameter-sets` injected as extradata
  when present (OBS and browsers also repeat SPS/PPS in-band). It is planned
  through the same HAL negotiation as every compressed ingest (**invariant #6**,
  [ADR-E001](ADR-E001.md)): decode-time/early downscale where the backend supports
  it, full-resolution software decode otherwise, the source's megapixels/sec
  charged to the decode budget either way. A **hard decode ceiling** applies:
  access units whose SPS declares more than **4096×2304 (≈8.8 Mpx)** are rejected
  — the session stays up, the frames are dropped with a rate-limited warn, and the
  tile shows an error/NO_SIGNAL state; under the ceiling, the source's
  **measured** Mpx/s enters the standard admission/degradation plan like any other
  source once observed.
- **`WebRtcProducer::to_produced` is fixed.** It feeds the decoder and stamps
  `FrameMeta` from the **decoder's output — geometry from the SPS**, `ColorInfo`
  from the H.264 VUI (BT.709 defaults when absent, the same policy as every other
  compressed ingest) — never from declared constructor arguments. The
  width/height constructor parameters go away.
- **B-frames are tolerated defensively.** Sane WHIP publishers send `bf=0` (OBS
  forces it; browsers do not B-encode), but a nonconforming publisher must not
  crash or corrupt: the decoder reorders to display order and the pump publishes
  in PTS order — the stream is handled, merely at higher latency.

### 5. Audio de-embed (Opus → the standard audio pipeline)

When `audio = true` the answer accepts one Opus m-line (RFC 7587, 48 kHz clock):

```
audio RTP → OpusDepacketizer (NEW, pure, multiview-input — payload is one
  Opus packet; a sequence gap surfaces as a discontinuity)
  → multiview-ffmpeg Opus decode → 48 kHz stereo f32 PCM
  → ADR-T013 RTP-audio rebase (declared 48 kHz clock → absolute frame index)
  → AudioStore (the source's entry in the pipeline `audio_stores` registry,
    crates/multiview-cli/src/pipeline.rs) → ProgramBus routing / breakaway
```

This is the first WebRTC consumer of the [ADR-T013](ADR-T013.md) seam — the
depacketizer owns only the codec step and the declared clock rate; wrap, anchor,
discontinuity re-anchor, reorder placement, and silence-fill are the shared
contract, never re-implemented here. `audio = false` answers the audio m-line
inactive and registers no store.

### 6. Timing (invariant #3)

The video RTP timestamp rides the 90 kHz clock and is surfaced **verbatim** as the
producer raw PTS; `WebRtcProducer::wrap_bits()` reports `WrapBits::Rtp32` so the
existing `PtsNormalizer` ([ADR-T003](ADR-T003.md),
`crates/multiview-input/src/normalize.rs`) unwraps the 32-bit wrap (~13.25 h at
90 kHz), applies the monotonic guard, and re-anchors on the depacketizer's
discontinuity flag. Audio follows the same algorithm at its own clock via
ADR-T013. The output clock re-stamps everything downstream — a publisher's
timestamps never pace anything (invariant #1).

### 7. Keyframe recovery: PLI policy + the documented OBS profile

The session sends **PLI** (RTCP Picture Loss Indication) on the publisher's video
SSRC: on session start and after a detected loss/discontinuity while the keyframe
gate is closed, **rate-limited to one per 2 s** — browsers answer a PLI with an
IDR within a frame or two, so a browser publisher goes LIVE near-instantly.
**OBS ignores PLI** (verified: its WHIP output's libdatachannel chain registers no
PLI handler), so recovery latency with OBS is bounded only by the *encoder's own*
keyframe interval. The in-app docs and the brief therefore state the supported OBS
publish profile: **keyframe interval 1–2 s, CBR** (OBS itself forces `bf=0` +
repeat-headers for WHIP). The keyframe gate (`H264Depacketizer`) holds delta frames
until the first IDR either way — corruption is never displayed (invariant #2).

### 8. Tile lifecycle + apply classification

- A configured-but-unpublished `webrtc` source shows the **NO_SIGNAL** placeholder
  — it is not "reconnecting"; there is nothing to dial.
- Session connected + first gate-opening IDR decoded → **LIVE**.
- Publisher disconnect / ICE timeout / idle GC ([ADR-0048](ADR-0048.md),
  `webrtc.session_idle_timeout`) → the standard [ADR-T002](ADR-T002.md) ride:
  STALE (hold last-good) → NO_SIGNAL, until the next publisher session arrives.
- Supervision: `drive_webrtc` in `multiview-cli` mirrors `drive_ndi`
  (`crates/multiview-cli/src/pipeline.rs`) — a non-dialing producer loop pumped by
  the ingest supervisor, pulling the `MediaEngine` and publishing through
  `IngestPump`.
- **Apply classification (invariant #11, [ADR-R004](ADR-R004.md)):** add/remove/
  enable/disable of a `webrtc` source is **Hot (Class-1)** — identical to every
  other network source (capability-matrix row *Source.lifecycle create/delete =
  Hot, drained off-thread*). `token`/`audio` edits are Hot with a session-side
  effect (the live session is closed and the publisher re-POSTs). The capability
  matrix gains the `webrtc` rows in the same push.
- Session counts, drops, and PLIs sent become Prometheus metrics
  (`multiview-telemetry`).

## Policy invariants

- **Sampled, never pacing (inv #1).** The producer only pulls (`poll_rtp` is
  non-blocking); a silent publisher yields `Ok(None)` and the tile holds
  last-good. No WHIP code path can block the output clock.
- **Bounded everywhere (inv #2; CLAUDE.md safety rule 5 / inv #9's bounded-queues
  clause).** The per-session RTP ring is drop-oldest
  (ADR-0048); the depacketizer's reorder window (`MAX_REORDER_PACKETS = 128`) and
  reassembly buffer (`MAX_ACCESS_UNIT_BYTES = 8 MiB`) drop, never grow; decoder
  frames come from the pooled decode path. Per-session memory is ≤2 MiB transport
  + up to 8 MiB in-progress access unit + decoder surfaces; the publisher fleet is
  bounded by the count of configured `webrtc` sources (one live session each,
  admitted outside the `webrtc.max_sessions` viewer pool — ADR-0048).
- **Isolation (inv #10).** WHIP terminates on the ADR-0048 endpoint task + the
  ingest threads; control's `WhipProvider` is synchronous and engine-free; a
  wedged endpoint loses ingest media, never output ticks.
- **One timing model (inv #3).** RTP 32-bit unwrap via the existing
  `WrapBits::Rtp32`/ADR-T003 path for video and the ADR-T013 seam for audio —
  no WHIP-private timestamp glue, no floats.
- **IPv6-first (ADR-0042).** The answer's candidates lead IPv6 (`c=IN IP6` for an
  IPv6 default candidate); examples use bracketed literals; the underlying socket
  binds dual-stack `[::]`.
- **Honest surface.** No stubbed methods: unsupported operations are `405`, an
  unwired build answers `503` — never a fake success.

## Consequences

- Browsers and OBS publish straight into a tile with no gateway, no restream hop,
  and no extra transcode — the contribution stream is decoded once, at the
  display-resolution plan, like every other source.
- WHIP audio lands in the standard `AudioStore` → `ProgramBus` path, so program
  mixing, breakaway, and metering treat a WebRTC contribution identically to an
  SRT or NDI source (and any ADR-T013 soak fix applies to it for free).
- The `405 PATCH` choice trades trickle-ICE away: a publisher behind a very slow
  candidate gatherer waits out its own gathering before POSTing. With host-only
  server candidates and no offered ICE servers this is sub-second in practice;
  revisiting it is additive (implement `PATCH`, drop the `405`).
- OBS recovery latency after loss is bounded by its keyframe interval (no PLI
  handling) — hence the documented 1–2 s profile; browsers recover via PLI within
  ~2 s worst-case (the rate-limit floor).
- A second publisher is refused (`409`) rather than pre-empting the live one —
  predictable on-air behaviour; the operator frees the slot via `DELETE` (or the
  publisher's own teardown/idle GC does).
- The scaffold's declared-geometry dishonesty is removed: geometry/colour now come
  from the bitstream (SPS/VUI), so a publisher that changes resolution mid-session
  simply yields new `FrameMeta` downstream.

## Alternatives rejected / considered

- **Require an external WHIP gateway** (publish to a third-party server, re-pull
  via RTSP/SRT). Adds a hop of latency and operational surface, usually a
  transcode, and makes the URL/token UX someone else's; a first-class source kind
  is the product shape. Rejected.
- **str0m sample-mode ingest** (let str0m depacketize H.264). Bypasses the
  already-proven, bounded, keyframe-gated `H264Depacketizer` and the pure
  `MediaEngine` seam its tests exercise — RTP mode keeps one depacketization
  contract for the pure and native builds. Rejected.
- **Implement trickle-ICE `PATCH`.** Spec-OPTIONAL for both ends; server→client
  trickle is forbidden by RFC 9725 regardless; vanilla ICE with pre-gathered host
  candidates answers fastest anyway. A stubbed `PATCH` (silent `204`) would be a
  lie. Rejected — honest `405`, revisitable additively.
- **Answer VP8 as well as H.264.** RFC 7742 obliges every browser to encode both,
  so H.264-only answers are universally serviceable, and VP8 would demand a second
  depacketizer + decoder path for no reach. Out of scope (stated boundary, not
  deferred work) — the egress side ships VP8 where it matters
  ([ADR-P006](ADR-P006.md)).
- **Anonymous publish when `token` is unset.** An open ingest port is an on-air
  vandalism vector; Write-scope API keys remain the floor. Rejected.
- **Delegate the WHIP server to libav.** libav's WebRTC support is a WHIP *output*
  muxer (client/publish side), not a server-side ingest demuxer — and a
  libav-owned socket would bypass the single shared endpoint mux
  ([ADR-0048](ADR-0048.md)). Rejected.
