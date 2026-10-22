# ADR-P006: WHEP preview completion — live egress, real encoders, audio, all scopes

- **Status:** Proposed
- **Area:** Preview
- **Date:** 2026-06-10
- **Source brief:** [webrtc.md](../research/webrtc.md), [preview-subsystem.md](../research/preview-subsystem.md)

## Decision

Complete the WHEP focus-preview path from negotiation scaffold to live, audible, all-scope preview
(work-schedule PRV-1c, PRV-5b, PRV-4b), in six moves. This ADR extends — and nowhere contradicts —
[ADR-P001](ADR-P001.md)/[ADR-P002](ADR-P002.md)/[ADR-P003](ADR-P003.md)/[ADR-P005](ADR-P005.md);
read [ADR-P005](ADR-P005.md) and [ADR-P002](ADR-P002.md) first, and [ADR-0048](ADR-0048.md) for the
shared transport endpoint this rides on.

**1. The native transport relocates to `multiview-webrtc`; preview keeps the pure seam.**
`multiview-preview`'s `webrtc-native` feature, its optional `str0m = "0.16.2"` dependency (a caret
requirement, lockfile-resolved; the new crate **tightens** it to an exact `=0.16.2` pin),
and `crates/multiview-preview/src/whep/native.rs` (`Str0mWhepTransport`, `parse_answer_attributes`,
plus `tests/whep_native.rs`) move into the new `crates/multiview-webrtc` crate behind its `native`
feature ([ADR-0048](ADR-0048.md)) — exactly **one** str0m owner per process, so WHEP egress shares
the single dual-stack `[::]` UDP socket, DTLS certificate, and driver task with WHIP ingest
([ADR-T014](ADR-T014.md)) and the WebRTC outputs ([ADR-0049](ADR-0049.md)). What **stays** in
`multiview-preview`, unchanged in role, is the pure `webrtc`-feature seam: `src/whep.rs`
(`WhepSession` offer parse / codec selection / `AccessScope::Focus` enforcement),
`src/whep/transport.rs` (`WhepTransport`, `TransportAnswer`, the bounded drop-oldest
`SampleSink`/`SampleFeed` pair, `PreviewMediaSource`, session ids/states), `src/whep/program.rs`
(`ProgramTap`, `ProgramFrame`, the `PreviewEncoder` trait, `ProgramFocusSource`/`ProgramFocusSession`,
`FidelityLabel`), and the in-memory fakes (`IdentityPreviewEncoder`; `FakeTransport`/`FakeMediaSource`
in `tests/whep_transport.rs`). The preview crate's `webrtc-native` feature is **removed**
(conventions §4 migration note); `multiview-cli`'s `webrtc-native` becomes
`["webrtc", "dep:multiview-webrtc", "multiview-webrtc/native", "ffmpeg"]`.

**2. Answer-SDP policy: str0m's full answer on the native path; the pure scaffold becomes honest
SDP.** A native session's HTTP answer is **str0m's own complete answer SDP** (correct BUNDLE,
`a=mid`, `a=rtcp-mux`, per-codec `a=fmtp`), not the hand-rolled rebuild that folds
`TransportAnswer` attributes into a template. The pure `WhepSession::build_answer` /
`build_answer_sdp` scaffold (`crates/multiview-preview/src/whep.rs`) **stays** — it is what the
fake-transport tests negotiate against — but its `c=IN IP4 0.0.0.0` placeholder lines become
`c=IN IP6 ::` (IPv6-first, [ADR-0042](ADR-0042.md)) and it gains `a=mid`/`a=rtcp-mux` lines, so the
fake path emits honest, structurally faithful SDP rather than a divergent dialect.

**3. The sample seam gains audio.** `EncodedSample`
(`crates/multiview-preview/src/whep/transport.rs`) gains
`kind: SampleKind { Video, Audio }`, with a per-kind RTP clock: video stays 90 kHz, audio is
**48 kHz** (the RTP clock RFC 7587 fixes for Opus). `PreviewMediaSource` gains an **optional audio
feed** (Opus samples) alongside the video `SampleFeed`; sessions whose offer carries no audio
m-line, and scopes with no audio source, simply leave it absent. This is a breaking seam change
made pre-1.0: every in-repo consumer — the fakes, `ProgramFocusSource`, the native transport — is
updated in the **same push**, never split.

**4. Real preview encoders (cli adapters, `ffmpeg`-gated) replace the identity encoder on the live
path.** Adapters in `multiview-cli` implement `multiview_preview::PreviewEncoder` over
`multiview-ffmpeg`: (a) **hardware H.264** chosen by the existing candidate-encoder selection
(`candidate_encoders`/`select_encoder`, `crates/multiview-ffmpeg/src/codec.rs` — NVENC/VAAPI/
VideoToolbox best-first, run-time resolved); (b) **VP8 via `libvpx`** as the always-available
LGPL-clean software rung (`VideoCodec` gains `Vp8`) — H.264 has no licence-clean software rung here (`libx264`
is `gpl-codecs`-blocked; BSD-licensed openh264 exists but is rejected, see Alternatives), while VP8
is RFC 7742-mandatory in every browser and str0m packetizes it;
(c) **Opus** for audio via `AudioCodec::Opus` (`libopus`, falling back to libav's native `opus`) at
48 kHz / 20 ms frames / 96 kbps — program scope consumes the shared program Opus rendition of
[ADR-0049](ADR-0049.md), which is lazy with refcount semantics mirroring the [ADR-P003](ADR-P003.md)
taps: it starts when the first audio-consuming WebRTC session or output attaches and stops with the
last (a configured `webrtc`/`whip_push` output with audio is a standing consumer). Encoder settings
are fixed for preview: zerolatency-class rate
control, **B-frames hard-off**, repeat-headers (SPS/PPS with every IDR), 2 s GOP, the preview
bitrate budget, input scaled to longest-edge ≤ 1280 (swscale) and sampled at ≤ 15 fps. Codec choice
at negotiate time is the intersection of the WHEP offer and what **this build** can encode — H.264
preferred, else VP8 — so the LGPL-clean-but-`ffmpeg` build still serves WHEP.

**5. All three scopes get frames; fidelity is labeled per [ADR-P005](ADR-P005.md).** *Program:*
the cli run loop that already fills the JPEG `ProgramSlot` (`crates/multiview-cli/src/preview.rs`)
also implements the `ProgramTap::subscribe` start-closure, publishing `ProgramFrame`s into a
dedicated drop-oldest `EventStream` ring (`crates/multiview-engine/src/isolation.rs`) at preview
cadence — running **only while ≥ 1 subscriber** (lazy start / auto-stop, [ADR-P003](ADR-P003.md)).
*Input:* sample the per-input `TileStore` last-good frame (the same read
`CliPreviewProvider::input_jpeg` uses) into a per-session encode. *Output, and program real
fidelity (PRV-5b):* when the tapped rendition is WebRTC-compatible — H.264 with B-frames off — the
preview registers a `PacketSink` on the `multiview-output::fanout` `PacketRouter` (one more sink on
`route()`, encode-once preserved, invariant #7 / [ADR-E003](ADR-E003.md)) and feeds the **real
encoded bitstream** to the session, labeled `FidelityLabel::RealEncodedOutput`; otherwise it falls
back to the canvas-approx encode path, labeled `PreEncodeCanvasApprox`. This extends the real-tap
preference of [ADR-P005](ADR-P005.md) to the program scope: a program focus may now carry
`RealEncodedOutput` when the program rendition itself qualifies (the `whep/program.rs` doc text
saying a program focus never uses that label is updated in the same push). SPS/PPS are cached and
prepended at each IDR so late joiners decode immediately. str0m `Event::KeyframeRequest` (PLI/FIR)
maps to a **rate-limited force-IDR callback** (≥ 2 s floor, coalesced) into the feeding preview
encoder — and, on the real-rendition path, into the program encoder's force-keyframe seam owned by
[ADR-0049](ADR-0049.md).

**6. Lifecycle, capabilities, degradation glue.** Sessions are garbage-collected on ICE disconnect
or idle timeout (`webrtc.session_idle_timeout`, default 30 s, [ADR-0048](ADR-0048.md)); closed-
session **tombstones are evicted after 60 s**, fixing the unbounded tombstone map the current
scaffold retains forever (`Str0mWhepTransport`'s closed entries). `GET /api/v1/preview/capabilities`
(in `multiview-control`, OpenAPI-registered and covered by the route-completeness assertion —
`ApiDoc::rest_routes` in `crates/multiview-control/src/openapi.rs` vs the mounted router in
`tests/openapi.rs`) returns
`{ "webrtc": bool, "scopes": { "program": { "whep": bool, "fidelity": "real-encoded-output" | "pre-encode-canvas-approx" }, "inputs": { "whep": bool }, "outputs": { "whep": bool } }, "fallback": "jpeg" }`
so the SPA picks its transport before POSTing an offer ([ADR-W023](ADR-W023.md)). `"jpeg"` is the
one fallback literal everywhere — the 503 problem-body hint uses the same value, and the shipped
`ws-jpeg` literal in `crates/multiview-control/src/routes/preview.rs` is renamed to `jpeg` in this
push (no consumer exists yet; the route tests update honestly). PRV-4b ships as
glue, not new machinery: the cli loop observes the degradation controller's `Hysteresis` steps
(defined in `crates/multiview-hal/src/degradation.rs`) and calls `FocusGate::suspend()`/`resume()`
(`crates/multiview-preview/src/focus.rs`), with tracing — preview WHEP is the first rung shed and
auto-resumes when pressure clears.

## Rationale

Relocating the native transport gives the process exactly one str0m surface: WHEP preview, WHEP
outputs, and WHIP ingest demux on one socket and one DTLS certificate, instead of each crate
growing its own UDP/ICE stack — and `cargo check --workspace` stays native-free because preview
keeps only the pure seam it already has. Taking str0m's own answer SDP eliminates the class of
hand-rolled-SDP interop bugs (missing `a=mid`, wrong fmtp) that every WHEP server project has hit,
while upgrading the retained scaffold to `c=IN IP6 ::` + mid/rtcp-mux keeps the fake-path tests
testing the same dialect browsers see. `SampleKind` is the minimal audio extension: one enum and
one clock rate, reusing the proven drop-oldest `SampleFeed` rather than inventing a parallel audio
seam. VP8/libvpx is the only software rung compatible with the LGPL-clean default licence posture
(`gpl-codecs` must stay opt-in, conventions §7) that browsers are required to receive; hardware
H.264 rides selection machinery that already exists. Feeding output-scope (and qualifying
program-scope) sessions the real packets is [ADR-P005](ADR-P005.md)'s own logic carried to WHEP:
zero extra encode via the existing fan-out, and the operator sees what consumers get — with the
mandatory label making approx unmistakable. The GC fix and the rate-limited force-IDR close two
unboundedness holes (memory in the tombstone map; bitrate under a PLI storm) that violate the
spirit of invariant #10 even though neither touches the engine. The capabilities endpoint exists
because the SPA must not discover WHEP absence by failing a POST — feature-gating plus capability
advertisement is exactly the contract [ADR-P002](ADR-P002.md) promised.

## Alternatives considered

Keeping str0m in `multiview-preview` and adding a second str0m to the ingest/output crates
(rejected: two sockets, two certs, two drivers, double the deny surface; demuxing WHIP+WHEP on one
port becomes impossible). Continuing the hand-rolled answer rebuild on the native path (rejected:
re-derives what str0m already emits correctly; every divergence is an interop bug). A separate
`AudioSample`/`AudioFeed` parallel seam (rejected: duplicates the ring, the fakes, and the tests
for one enum's worth of difference). x264 as the software encoder rung (rejected: flips the build
to GPL — licence-blocked by conventions §7). BSD-licensed openh264/libopenh264 as the software
H.264 rung (rejected: building from source falls outside Cisco's binary-only patent grant, leaving
H.264 patent-royalty exposure, and it adds a runtime dependency — VP8/libvpx stays the software
rung). Shipping video-only WHEP and deferring audio
(rejected: lipsync/confidence monitoring is a core focus-preview purpose, and the seam change is
cheapest now, pre-1.0). Per-session program encodes for real fidelity (rejected: forbidden by
encode-once, [ADR-E003](ADR-E003.md); the fan-out tap costs one routing-table entry). Forwarding
every PLI to the encoder unfiltered (rejected: a misbehaving viewer could force an IDR per RTT and
inflate program bitrate — rate-limit at ≥ 2 s and coalesce). Trusting DELETE alone for session
teardown (rejected: leaks on abrupt drops; [ADR-P003](ADR-P003.md) already mandates
connect+timeout+watchdog teardown — the tombstone eviction completes it).

## Consequences

WHEP preview becomes live end-to-end on `webrtc-native` builds (all release presets carry it),
with audio, on program/input/output scopes, falling back to the JPEG path on pure builds — the
SPA's WHEP→JPEG ladder ([ADR-W023](ADR-W023.md)) keys off the capabilities endpoint; the LL-HLS
fallback rung of [ADR-P002](ADR-P002.md) remains a separate schedule item, unaffected. The
`EncodedSample`/`PreviewMediaSource` seam change touches every consumer in one push (fakes, native
transport, focus wiring) — mechanical but not optional. Preview cost stays bounded and idle-free:
≤ 1 encode per focus (FocusGate caps), ≤ 15 fps at ≤ 1280 longest edge, per-session memory ≈ ≤ 2 MiB
across str0m buffers plus two drop-oldest feeds, and the new engine→outside channels (the
`ProgramFrame` `EventStream` ring, the audio ring) are drop-oldest and covered by prove-no-stall
tests — the structural isolation model of [ADR-P001](ADR-P001.md) (read-only taps, drop-oldest,
shed-first) is reused unchanged, so invariant #10 holds structurally and invariant #1 is untouched
(nothing here runs on the tick loop). [ADR-P005](ADR-P005.md) remains valid: its labeling doctrine now also covers program
WHEP, where `RealEncodedOutput` becomes reachable; `multiview-preview` loses its `webrtc-native`
feature (downstreams must move to `multiview-webrtc/native`), and `deny.toml` comments plus the
work-schedule notes (PRV-1b completed-by-relocation; PRV-1c/PRV-4b/PRV-5b delivered here) are
updated accordingly.
