# ADR-I005: One packet carrier — converge the two `EncodedPacket` types on `multiview_ffmpeg::EncodedPacket` as the muxed-egress carrier, with a documented byte-payload adaptation seam for the GStreamer RTSP egress (RT-12)

- **Status:** Accepted
- **Area:** Implementation Build-out
- **Date:** 2026-06-10
- **Source brief:** [decoupled-routing.md](../research/decoupled-routing.md), [iso-program-recording.md](../research/iso-program-recording.md), [efficiency.md](../research/efficiency.md)
- **Realizes:** Invariant #7 (encode-once-mux-many), invariant #3 (re-stamp from the tick), invariants #1/#10 (the fan-out never stalls or back-pressures the engine); see [ADR-0026](ADR-0026.md), [ADR-E003](ADR-E003.md), [ADR-E004](ADR-E004.md), [ADR-0006](ADR-0006.md), [ADR-0034](ADR-0034.md), [ADR-0037](ADR-0037.md)
- **Backlog item:** RT-12 (output identity + `OutputRef` + **converge `EncodedPacket`** + runtime sink-mover) — see the [decoupled-routing backlog](../development/decoupled-routing-backlog.md). RT-13 (the egress splicer) is a **separate** item that builds on this.

## Context

The codebase grew **two** distinct "one encoded packet" carriers, and inv #7
(encode the canvas once per rendition, fan the *same* coded packets to N
transports) was being documented twice, differently. **Both carriers are wired
into production egress paths today** — neither is dead. They differ because the
two transport families they feed want the coded payload in genuinely different
shapes:

1. **`multiview_ffmpeg::EncodedPacket`** (`crates/multiview-ffmpeg/src/packet.rs`)
   — a `Send` wrapper around one libav `codec::packet::Packet` (an `AVPacket`),
   tagged with a `StreamKind` (`Video` / `Audio`, `#[non_exhaustive]`). It exposes
   `pts`/`dts`/`is_keyframe`/`len`, yields an **independently-writable owned copy**
   per consumer via `to_owned_packet()` (`av_packet_ref` + `av_packet_make_writable`)
   and a move-out `into_owned_packet()` for the last muxer. This is the carrier for
   the **libav-muxed** egress family: `ProgramEncoder::encode_frame` /
   `encode_audio*` (`multiview-output/src/sink.rs`) emit `Vec<EncodedPacket>`; the
   CLI `consumer_main` / `fan_packets` (`multiview-cli/src/pipeline.rs` ~L2629–2775)
   clone one per sink across bounded `sync_channel`s, and each `PacketMuxSink` pulls
   them through `PacketSource::next_packet` → `drive_packets_to_single_muxer`
   (`sink.rs` ~L915–935), which routes by `kind()` to the right muxer
   `stream_index` and writes **its own owned copy** so the muxer's in-place
   `set_stream` + `rescale_ts` per stream stays sound. The file, LL-HLS, and
   RTMP/SRT-push sinks all sit on this carrier.

2. **`fanout::EncodedPacket`** (`crates/multiview-output/src/fanout.rs`) — a
   pure-Rust struct `{ rendition, kind: PacketKind, pts, dts, duration, data: Arc<[u8]> }`
   handed out as one `Arc<EncodedPacket>` and fanned **by reference** through a
   `PacketSink` trait. This is **not dead**: it is the carrier for the
   **byte-stream** egress family. The in-process RTSP server (OUT-2, ADR-0006
   primary path — a ~915-line always-compiled seam: `rtsp_server/{sink.rs,
   queue.rs, server.rs, mount.rs, caps.rs}`) is built on it: `RtspServerSink`
   **implements `fanout::PacketSink`**, its `deliver` is a non-blocking push into a
   bounded drop-oldest `BoundedPacketQueue` of `Arc<EncodedPacket>`, and the
   GStreamer serving side (`server.rs`, behind the `rtsp-server` feature) drains
   that queue and pushes the **already-encoded NAL bytes** into a
   `appsrc → h264parse → rtph264pay` pipeline **zero-copy** over the shared
   `Arc<[u8]>` (`build_buffer` / `PacketBytes`). The `EncodeOnceDriver` /
   `PacketRouter` over this trait are *also* the GPU-free encode-once **proof
   harness** (the call-count spy in `tests/encode_once_sink_mover.rs`).

### Why a single "promote one, delete the other" move is wrong

An earlier draft of this ADR claimed `fanout::EncodedPacket` was "dead/unwired in
production" and that "every production sink ultimately hands bytes to a libav
muxer", and on that basis proposed retiring the `Arc<[u8]>` carrier outright.
**Both claims are false against the as-built tree** and the conclusion does not
follow:

- The `Arc<[u8]>` carrier has a **real, always-compiled production consumer** —
  `RtspServerSink` and the GStreamer RTSP egress. What is *not* yet wired is the
  CLI plumbing: `pipeline.rs` skips `Output::RtspServer` with a "not implemented"
  warning (~L3814), so the sink is built but the CLI does not yet construct and
  register it. That is an **OUT-2 wiring gap**, not a dead type — the carrier and
  its consumer both exist and are exercised on CI (the seam) and on a
  GStreamer-equipped runner (the live serve).
- The RTSP egress **does not hand bytes to a libav muxer at all.** It serves NALs
  directly: GStreamer's `h264parse`/`h265parse` only fix up stream-format and
  `rtph264pay`/`rtph265pay` do RTP packetisation. `appsrc` wants **raw bytes**, so
  an `AVPacket` would have to be *un*wrapped to its payload at that boundary — the
  `Arc<[u8]>` shape is the *right* one there, and forcing the RTSP path onto an
  `AVPacket` would add an unwrap, not remove a wrap. NDI out (when wired) is the
  same family: it consumes raw compressed/uncompressed buffers, not `AVPacket`s.

So there are **two egress families with two natural payload shapes**:

- **libav-muxed** — file, LL-HLS, MPEG-TS, RTMP/SRT push. Each muxer mutates an
  `AVPacket` in place (`set_stream` + `rescale_ts` per stream), so it needs an
  owned, writable `AVPacket` and an `Arc<[u8]>` would be re-wrapped per sink.
- **byte-stream** — the GStreamer RTSP egress (and future NDI). Each consumes the
  coded NAL **bytes**; an `Arc<[u8]>` is fanned zero-copy and an `AVPacket` would
  be unwrapped per sink.

Having **two unrelated carrier types** for the same logical "one encoded packet,
fanned to N transports" is still a latent defect — the two diverge in tag
taxonomy (`StreamKind{Video,Audio}` vs `PacketKind{VideoKeyframe,VideoDelta,Audio}`)
and in what "encode once" is documented to mean. Three gates are about to be built
that all fan packets and would each independently pick (or re-invent) a carrier,
entrenching the split permanently if convergence is not pinned **first**:

- **AUD-4 dual-stream mux** — routes `StreamKind::Audio` vs `Video` to the muxer's
  audio/video stream index (`stream_index_for`, `sink.rs`).
- **OUT-2 RTSP wiring** — connect `RtspServerSink` into the CLI fan-out (close the
  `pipeline.rs` gap) so the RTSP egress receives the program's coded packets.
- **PRV-5 program preview tap** — a program `PacketSink`.

`fanout::PacketKind` (`VideoKeyframe`/`VideoDelta`/`Audio`) and
`multiview_ffmpeg::StreamKind` (`Video`/`Audio`) are **two taxonomies for the same
routing decision**. The keyframe/segment-boundary bit is a *packet flag*
(`is_keyframe()`), not a stream identity, so folding it into the kind enum
conflates "which elementary stream" with "is this a segment boundary".

## Decision

**Converge on one carrier type and one routing taxonomy, while keeping the
byte-payload adaptation the GStreamer RTSP egress needs explicit.** The carrier is
`multiview_ffmpeg::EncodedPacket` (the `AVPacket` wrapper) for everything that
crosses the fan-out; the byte-stream egress family adapts to its raw payload at a
single documented seam rather than carrying a second top-level type.

Concretely:

1. **One fan-out carrier.** `multiview_ffmpeg::EncodedPacket
   { packet: codec::packet::Packet, kind: StreamKind }` is *the* type that flows
   across the fan-out's bounded channels. It is `Send`, movable, and `Clone` is a
   ref-counted `av_packet_ref` that **preserves the `kind` tag**. The libav-muxed
   sinks take an owned writable copy via `to_owned_packet()` (or `into_owned_packet()`
   for the last); the byte-stream sinks read the coded payload via a `&[u8]` view.

2. **One routing taxonomy.** `StreamKind` (`Video` / `Audio`, `#[non_exhaustive]`)
   is the single tag. Muxed sinks pick the destination `stream_index` from it
   (`stream_index_for`: video→video, audio→registered audio, else a typed error —
   never a silent mis-route). The **keyframe / segment-boundary** decision is read
   from the packet flag (`EncodedPacket::is_keyframe()`), *not* from the kind — a
   segmenter rotates on `kind == Video && is_keyframe()`
   (`drive_packets_to_single_muxer`, `sink.rs`). `fanout::PacketKind` is removed;
   its `VideoKeyframe`/`VideoDelta` split is expressed as `StreamKind::Video` + the
   keyframe flag.

3. **Byte-stream adaptation seam (the RTSP/NDI shape).** The GStreamer RTSP egress
   and NDI out do **not** want an `AVPacket` — they want the coded NAL bytes. So
   the `PacketSink` trait the byte-stream egress implements takes the converged
   `EncodedPacket` and reads its **payload by reference** at the seam:
   `EncodedPacket` gains a `payload(&self) -> &[u8]` (a borrow of the
   `AVPacket`'s data; **no copy**) plus the already-tick-stamped `pts`/`dts`/
   `duration`/`is_keyframe()` the pump needs. `RtspServerSink::deliver` enqueues a
   cheap clone of the carrier (a ref-counted `av_packet_ref`, header-only) into its
   bounded `BoundedPacketQueue`, and the GStreamer pump builds the `gst::Buffer`
   from the payload view — still zero-copy on the bytes (the `gst::Buffer` holds
   the carrier alive and borrows its data), preserving the current `PacketBytes`
   behaviour. This keeps **one** carrier type while honouring that `appsrc` wants
   raw bytes: the `Arc<[u8]>` is not a *second carrier*, it is the *payload shape*
   at one boundary, derived from the single carrier.

   *Crate-direction note:* `multiview-output` already depends on `multiview-ffmpeg`
   under the `ffmpeg` feature (it names `EncodedPacket`/`StreamKind` in `sink.rs`
   today), so re-pointing the `PacketSink` trait at the converged carrier does not
   add a dependency, and `multiview-output` stays `unsafe_code = forbid` — it
   never constructs a raw `AVPacket`; it only borrows the payload via the safe
   `payload()` accessor the wrapper exposes.

4. **What "encode once" means, stated once.** Inv #7's load-bearing claim is **the
   encoder runs once per rendition per tick**; the per-sink hand-off is a
   refcounted header copy over a shared payload (`AVBufferRef`). The libav-muxed
   sinks each get their own writable header for in-place `rescale_ts`; the
   byte-stream sinks share the payload by reference. The `fanout.rs` module doc's
   "Two `EncodedPacket` notions" section is deleted and replaced with this single
   reconciliation plus a link to this ADR.

5. **Routing-model harness role, retained.** `PacketRouter` / `EncodeOnceDriver` /
   `RenditionEncoder` / the `PacketSink` trait stay as the in-crate, GPU-free
   encode-once **proof harness** (the call-count test is load-bearing for inv #7
   and runs on CI without libav). They now operate over the converged carrier (or,
   where the harness only needs the *count* and the routing decision rather than a
   real payload, a `#[cfg(test)]` `PacketMeta { rendition, kind: StreamKind, pts,
   dts, duration, is_keyframe }` descriptor). The runtime sink-mover
   (`PacketRouter::register`/`deregister`/`move_sink`) keeps operating on
   `Arc<dyn PacketSink>` route entries — a routing-table operation independent of
   which packet type flows — so the RT-12 sink-mover and the carrier convergence
   compose cleanly.

## Migration

Docs-only ADR; this pins the design the three gates build against. The mechanical
migration (separate, test-first PRs, no behaviour change to the production muxed
path which **already** carries `multiview_ffmpeg::EncodedPacket`):

1. **Add the payload accessor.** `multiview_ffmpeg::EncodedPacket::payload(&self)
   -> &[u8]` (and the timestamp accessors the pump needs if any are missing),
   covered by a unit test that asserts it borrows the same bytes the encoder
   produced.
2. **Re-point the `PacketSink` trait at the converged carrier.** Change
   `fanout::PacketSink::deliver` to take the `multiview_ffmpeg::EncodedPacket`
   carrier; update `RtspServerSink`/`BoundedPacketQueue` to carry it and the
   GStreamer pump to build its `gst::Buffer` from `payload()`. The live
   `tests/rtsp_server_playout.rs` and the seam tests assert the bytes/timestamps
   are unchanged.
3. **Collapse `PacketKind` → `StreamKind` + keyframe flag.** Re-express the
   routing-model harness over `StreamKind` + `is_keyframe: bool`, removing
   `fanout::PacketKind`; update the call-count test to assert the keyframe-boundary
   path off the flag, not the enum variant.
4. **Delete the divergence doc.** Replace `fanout.rs`'s "Two `EncodedPacket`
   notions" module-doc with the single reconciliation (decision §4) and a link to
   this ADR.
5. **OUT-2 wires the RTSP egress.** Close the `pipeline.rs` `Output::RtspServer`
   gap: construct the `RtspServerHandle` + `RtspServerSink`, register it as a
   fan-out sink, and feed it the program's coded packets — over the converged
   carrier, no new type. AUD-4 and PRV-5 likewise build against the pinned carrier.
6. **`cargo build --workspace` stays green** at every step (the muxed production
   path is unchanged; the RTSP seam's carrier type is swapped behind its public
   surface).

## Alternatives considered

- **Retire `fanout::EncodedPacket` (`Arc<[u8]>`) as "dead/not a production carrier"
  and promote only the `AVPacket` wrapper** (the earlier draft of this ADR) —
  **rejected as factually wrong.** The `Arc<[u8]>` carrier is the live RTSP egress
  carrier (`RtspServerSink` + the GStreamer pump); only the CLI *wiring* of that
  output is incomplete. And the RTSP path serves NALs through GStreamer's RTP
  payloaders, **not** a libav muxer, so the "every sink hands to a libav muxer"
  premise is false. The right fix converges the *type* while keeping the
  byte-payload shape that egress family needs (decision §3), not deletes a used
  type.
- **Back `multiview_ffmpeg::EncodedPacket` with `fanout::EncodedPacket`'s
  `Arc<[u8]>`** (the RT-12 backlog's first option) — **rejected** for the
  libav-muxed family. Those muxers rescale timestamps **in place** per stream, so a
  shared immutable byte buffer cannot serve N muxers; each would copy the bytes
  into a fresh `AVPacket` before libav could write them, *adding* a full-payload
  copy per sink that the `AVBufferRef` refcount already avoids, and pushing
  `AVPacket` construction into the `unsafe_code = forbid` `multiview-output` crate.
- **Keep both carriers, document the split honestly** (the RT-12 fallback, the
  current state) — **rejected as the end state.** It is exactly the divergence the
  three gates would entrench (and it leaves two tag taxonomies); "honest
  documentation of a follow-up" is the deferred-debt anti-pattern, and convergence
  is now unblocked.
- **Define a third, neutral carrier in `multiview-core`** (an `Arc`-wrapped coded
  buffer + `StreamKind` + timestamps, libav-agnostic) and adapt both ends —
  **rejected.** The libav-muxed family (file/HLS/TS/RTMP/SRT — the majority of
  sinks) ultimately hands an `AVPacket` to a libav muxer, so a neutral type forces
  a re-wrap into `AVPacket` at every muxed boundary; the `AVPacket` wrapper already
  *is* the `Send`, refcounted carrier those sinks need, and §3's `payload()` view
  gives the byte-stream family its raw bytes without a third type.
- **Promote `fanout::PacketKind` (with the keyframe variant) as the tag** —
  **rejected**: it conflates "which elementary stream" (a stable routing identity)
  with "is this a segment boundary" (a per-packet flag), and would force every
  audio packet to be re-classified and every video packet to carry its GOP position
  in its *kind*. `StreamKind` + `is_keyframe()` keeps the two orthogonal.

## Consequences

- **One carrier, one taxonomy.** AUD-4, OUT-2, and PRV-5 all fan
  `multiview_ffmpeg::EncodedPacket` and route on `StreamKind`; no gate re-invents a
  carrier, so they cannot diverge.
- **The byte-stream egress shape stays first-class and honest.** The GStreamer RTSP
  egress (and future NDI) still get the coded payload as raw bytes, zero-copy, via
  the documented `payload()` seam — the convergence does not pretend every sink is a
  libav muxer.
- **Inv #7 is stated once, correctly.** "Encode once per rendition per tick; fan a
  refcounted header copy over a shared payload — own writable header per libav
  muxer, payload-by-reference for byte-stream sinks." The duplicate/contradictory
  `fanout.rs` framing is gone.
- **`multiview-output` stays `unsafe_code = forbid`** — it borrows the payload via
  the safe `payload()` accessor and never constructs a raw libav packet; the libav
  type stays owned inside `multiview-ffmpeg`.
- **The routing-model harness keeps proving inv #7 GPU-free on CI** (call-count
  spy), now over the converged carrier (or a `#[cfg(test)]` `PacketMeta`).
- **RT-12 (this convergence + output identity + sink-mover) and RT-13 (the egress
  splicer) compose**: the sink-mover is a routing-table re-key over
  `Arc<dyn PacketSink>` entries, independent of the packet type that flows; RT-13's
  `RestampAccumulator`-straddled program↔program splice runs over the single
  carrier. ADR-0037 (ISO recording, which reuses the RT-12 fan-out sink-mover and
  the GP-7 copy path) and ADR-0034's egress lane inherit the single carrier.
- **No behaviour change to the running muxed pipeline** — it already carries this
  type; the migration swaps the RTSP seam's internal carrier behind its public
  surface and removes the duplicate tag enum + the divergence doc. Closing the
  `pipeline.rs` `Output::RtspServer` wiring gap is OUT-2's job, scheduled against
  the pinned carrier.
