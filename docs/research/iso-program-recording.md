# ISO + Program recording — faithful capture to disk that can never stall the engine

> **Status:** research brief (verification-hardened). **Area:** Output / Input / Engine /
> Config / Telemetry / Control / Web. **Decision:** [ADR-0037](../decisions/ADR-0037.md).
> **Builds on:** GP-7 guarded passthrough ([ADR-0030](../decisions/ADR-0030.md)),
> the HLS live segmenter ([ADR-0032](../decisions/ADR-0032.md)), encode-once-mux-many
> ([ADR-0026](../decisions/ADR-0026.md)), self-aware health warnings
> ([ADR-0035](../decisions/ADR-0035.md), [self-aware-placement](self-aware-placement.md)),
> resilience/back-off ([resilience-and-av](resilience-and-av.md), [ADR-R003](../decisions/ADR-R003.md)),
> and HLS delivery ([hls-delivery](hls-delivery.md)).
> **Invariants in play:** #1 (output-clock never stalls), #7 (encode-once-mux-many),
> #3 (timing), #10 (isolation — best-effort consumers cannot back-pressure the engine).
> This brief states the *why*; it references the invariants and ADRs rather than
> duplicating them.

---

## 0. Terminology (industry broadcast terms — used everywhere in this design)

This design adopts the standard production-truck recording model. The vocabulary is
load-bearing: it shapes the config keys, the API paths, the realtime event types, and
the UI.

- **ISO** — *Isolated recording.* A **continuous, independent** recording of a **single
  source/input**, captured **regardless of whether that source is currently on-air** (in
  the multiview). A truck records an ISO of every camera so the show can be re-cut later.
  This is the operator's *"raw input to disk"* half. **Per input.**

  > **Disambiguation (in the docs deliberately, so nobody confuses three "ISO"s):** here
  > **ISO = Isolated** (the broadcast term). It is **not** ISO-639 language codes (already
  > modelled at `crates/multiview-config/src/audio.rs`), and **not** ISO-BMFF / `mp4`. When
  > this brief says "ISO recording" it always means the isolated per-source capture.
  > (Pleasing coincidence: *isolated* is also exactly the isolation property invariant #10
  > demands of a best-effort sink.)

- **Program / PGM** — a recording of the **switched / composited OUTPUT** (the multiview
  program, or a specific rendition). This is the operator's *"raw output to disk"* half.
  **Per output / rendition.**

- **CLEAN vs DIRTY** — a **clean** program feed is the composited program **without**
  overlays/graphics (before the keyer); a **dirty** feed has graphics **burned in**, as
  produced. v1 records the **dirty** program (it is free — see §B). Clean is a noted future
  axis, *not* v1.

- **Arm / disarm** — an ISO or Program recorder is **armed** (recording) or **disarmed**
  (idle). Arm/disarm is an operational toggle, distinct from editing the recording *config*.

We use **ISO** and **Program** throughout — not a bespoke "raw"/"archive" vocabulary. The
words *raw/archival* appear once here only to connect to the operator's phrasing.

---

## 1. What the operator asked for

> "raw INPUT to disk" + "raw OUTPUT to disk" — take the input **as is** (transport, every
> elementary stream, everything) and write it to disk; same for the output. With
> **rotation** (keep 30 min / 1 h / 7 days / …), a **disk-availability policy** (stop
> writing at e.g. 80 % full), and it **must warn + back off** so it does **not** take out
> core functionality (a failing write → warn, wait, retry — never crash, never stall).

In industry terms: an **ISO** recording per input (faithful, all elementary streams,
codec-copy) **plus** a **Program** recording per output (the composited program), both
**segmented + retained + disk-pressure-gated**, and — the cardinal constraint — both
**physically incapable of back-pressuring the engine or the live outputs.**

---

## 2. The cardinal constraint (load-bearing)

A recorder — ISO or Program — is a **best-effort sink that is physically incapable of
back-pressuring the engine or the live transports.** This is invariant #1 (the output
clock emits one valid frame per tick forever, independent of any consumer) and invariant
#10 (best-effort consumers use bounded **drop-oldest** queues; the engine **never awaits
them**) applied to a disk sink.

A slow / full / unmounted / erroring disk causes the recorder to:

1. **DROP** (its bounded ring overflows — drop, count, continue),
2. **WARN** (a health warning with a concrete remediation string), and
3. **BACK OFF** (capped-exponential retry with full jitter),

and to **auto-resume** when the disk is healthy again. It **never** blocks the data plane,
**never** unbounded-buffers into RAM, and **never** panics/crashes. A failing recording
degrades to *"paused/disarmed with a clear warning"* — **never** to *"output faltered."*
The whole point of the name: **ISO = isolated** literally satisfies the isolation
requirement.

This brief's §E and the chaos gate make that guarantee **structural and tested**, not
aspirational (an adversarial review found the engine's *production* fan-out does **not**
today use a non-blocking drop-oldest hand-off everywhere — see §B — so the recorder must
interpose its **own** ring; it must not inherit the existing block-then-mark-dead path).

---

## 3. As-built vs missing (grounded in code)

The design **reuses** existing, tested machinery wherever possible and flags exactly what is
net-new. The adversarial review corrected several "it already does X" overstatements; this
table reflects the corrected truth.

| Capability | As-built (reuse) | Missing (build) |
|---|---|---|
| **Read all elementary streams** | `Demuxer::read_packet()` returns a `ReadPacket{stream_index, packet}` for **any** stream (`crates/multiview-ffmpeg/src/demux.rs`). | Current ingest is **video-only**: `FileSource::next_frame` → `read_packet_for(video_index)` (`crates/multiview-input/src/libav.rs`), and `read_packet_for` **silently discards** every non-target packet (`demux.rs`). ISO needs a **packet-router on the single input read thread** (§A). |
| **Enumerate streams** | `Demuxer::inventory()` → typed `StreamInventory` (video/audio/subtitle/data·SCTE-35·KLV/timecode) (`demux.rs`, `crates/multiview-core/src/stream.rs`), enriched by `merge_ts`/`merge_hls` (`crates/multiview-input/src/inventory.rs`). | `StreamInventory` carries *descriptors*, **not** the libav `AVCodecParameters` a copy stream needs. |
| **Register a copy (encoder-less) stream** | `Muxer::add_stream_from_parameters` does `avcodec_parameters_copy` onto an encoder-less stream (`crates/multiview-ffmpeg/src/mux.rs`); `write_packet` rescales+interleaves; `create_with_options` sets `avoid_negative_ts`/`max_interleave_delta`; `finish` writes the trailer idempotently. | It accepts a `StreamCodecParameters` whose **only** constructors are `from_encoder`/`from_audio_encoder` (`crates/multiview-ffmpeg/src/packet.rs`) — both snapshot an **opened encoder**. **No constructor snapshots a demuxed input stream.** Build `StreamCodecParameters::from_parameters(&codec::Parameters)` fed by `Demuxer::stream_parameters(index)`. |
| **Program file sink (single encode)** | `PacketMuxSink` holds **no encoder** — it is the packet-fed, mux-only egress of inv #7 (`crates/multiview-output/src/sink.rs`); `maybe_prepend_program_ts` already prepends a `PacketMuxSink::file("program.ts")` over the **same** fanned packets (`crates/multiview-cli/src/pipeline.rs`) — the v1 precedent. | That precedent is **single-file, not rotating**, and its fan-out coupling is **blocking-with-timeout** (`send_bounded`/`SINK_WEDGE_GRACE`), **not** drop-oldest auto-resuming. The recorder must add a **rotating** sink behind its **own** bounded ring (§E). |
| **Keyframe-rotated segmenting + atomic publish** | `PacketMuxSink::segment_live` rotates on a **video keyframe** (`is_keyframe()` — never a GOP counter); `close_current` publishes a segment via same-dir `<seg>.tmp` → `rename(2)`; `LivePlaylist` evicts beyond a count window and re-publishes the manifest via `atomic_write` (tmp → `sync_all` → `rename`) (`crates/multiview-output/src/hls/{live,media}.rs`). | Two gaps: (1) **prune is count-window only** — need **dual time-AND-size** retention (§C); (2) the **segment media file is NOT fsync'd** before rename — only the *manifest* is (`live.rs` fsyncs, `close_current`→`write_trailer`→`rename` does **not**). State this honestly and/or add a segment `sync_all` (§C). |
| **Restamp guard across seams** | `RestampAccumulator` (GP-6, `crates/multiview-output/src/restamp.rs`) clamps DTS monotonic per emitted packet and re-anchors the offset at a seam — used today only in `guarded.rs`. | Not yet wired into the **segment** writer; ISO needs a **per-ISO-stream** `RestampAccumulator` as the `av_interleaved_write_frame` abort-guard (§A). |
| **Sink re-keying at a frame boundary** | RT-12 `move_sink` re-keys a warm sink between renditions with **zero** extra encode (`crates/multiview-output/src/fanout.rs`). | (Routing-model path; the **production** fan-out is `fan_packets`/`sync_channel`.) `move_sink` does **not** encode — it cannot synthesize a clean feed (§B). |
| **Disk free-space sensing** | — | **Nothing exists** (`rg statvfs|f_bavail|free_space crates/` → 0; `availability.rs` is ITU-T uptime-ratio, not disk). Build a `statvfs(2)` sensor in a new `*sys` crate consumed by `multiview-telemetry` (§D). |
| **Health warnings** | **Fully implemented**: `HealthWarning`/`WarningCode`/`WarningSeverity` + `Event::HealthWarningRaised`/`Cleared` (`crates/multiview-events/src/event.rs`), `WarningRepository`+`InMemoryWarningStore`, the engine→control lagged-skip ingest, `GET /api/v1/health`, and the web `HealthBanner` (`crates/multiview-control/src/{warning_store,warning_ingest,routes/health}.rs`, `web/src/components/HealthBanner.tsx`). | `WarningCode` has **one** variant today (`GpuPresentNoVulkanAdapter`). It is `#[non_exhaustive]`; just **add four additive variants** (§ warnings). **No** prerequisite gap; **do not** interim-ship as `Alert` (it lacks `code`/`subsystem`/`remediation`). |
| **`backon` back-off crate** | resilience-and-av mandates `backon` (the `backoff` crate is unmaintained — **RUSTSEC-2025-0012**). | `backon` is **not yet a workspace dep** (`rg backon crates/` → 0). Add it (feature-gated, `cargo deny`-clean) as part of this work. |
| **Byte-exact on-wire passthrough** | — | The only custom AVIO is `avio_fetch.rs` (read-only text). Byte-exact TS/SRT needs a **new raw-AVIO/socket tee** that does not exist. **Deferred** (§A). |

---

## A. ISO recording — faithful per-source capture of ALL elementary streams (codec-copy)

**Tap point = the demuxer, never the decode path.** Today's ingest reads **video-only**:
`FileSource::next_frame` calls `read_packet_for(self.video_index)`
(`crates/multiview-input/src/libav.rs`), and `read_packet_for` **silently discards** every
non-target packet (`demux.rs` — the `Some(_) => {}` arm drops audio/subtitle/data/SCTE-35/
timecode and reads again). The ISO recorder forks at `Demuxer::read_packet()`, which returns
a `ReadPacket{stream_index, packet}` for **any** stream.

This must be a **packet-router on the single input read thread**, because a libav demuxer
context is `Send + !Sync` and **read-once** over one `AVFormatContext` (safety rule §7.4):
once the video-only loop has consumed-and-discarded a non-video packet it is *irrecoverably
gone*, and a second independent demuxer is **not** an option — live single-connection
transports (SRT/RTSP/RTMP) cannot be opened twice, and a second demuxer breaks the
single-read-thread / `!Sync` posture. So the router:

1. runs **one** `read_packet()` loop (replacing `read_packet_for(video_index)`),
2. hands the **video-stream-index** packet to the existing decode/ingest path **unchanged**
   (preserving today's exact tap behaviour — this must remain the **unblocked primary**), and
3. clones **every** packet — via `av_packet_ref` (the same ref-count primitive
   `to_owned_packet` uses, `packet.rs`) — into the ISO writer's **bounded drop-oldest ring**
   (§E). The clone can never delay or back-pressure decode.

**Mechanism = faithful copy-REMUX for v1, across ALL transports.** Stream-copy every
elementary stream (no re-encode) into a rotating container (`mkv` preferred — carries
arbitrary codecs incl. data/timecode; `ts` for TS-native sources) via the safe `Muxer`.
The copy primitive is real and verified: `add_stream_from_parameters` attaches **no**
encoder and does `avcodec_parameters_copy`; `write_packet` only does `rescale_ts` +
`write_interleaved` (no decode/encode round-trip). **But the wiring is net-new** (the
adversarial review refuted "every inventory stream already survives to disk"): the only
`StreamCodecParameters` constructors snapshot an **opened encoder**, and `StreamInventory`
carries *descriptors*, not `AVCodecParameters`. So this work must:

- add `StreamCodecParameters::from_parameters(&codec::Parameters)` (mirroring `from_encoder`'s
  copy-into-fresh-alloc) fed by `Demuxer::stream_parameters(index)`;
- register **one output stream per `StreamInventory` row** before `write_header`;
- prove with a TDD test that demuxing a **multi-stream TS** (video + audio + subtitle +
  SCTE-35/data) yields one output stream per input index with **matching `codec_id` and
  extradata length**, and that **no decoder/encoder is instantiated.**

`StreamInventory` is the existing RT-1/RT-2 surface (`demux.rs`, `core/src/stream.rs`),
enriched by `merge_ts`/`merge_hls` with PMT languages + authoritative SCTE-35 PIDs + HLS
alt renditions. A JSON sidecar `index.json` (atomically published) records the
`StreamInventory` + per-segment first-PTS for retrieval.

**Routing keying.** Do **not** extend the encode-once `StreamKind` enum (it is `Video|Audio`
only and is the *program* routing key). Build a **dedicated `IsoMuxSink`** keyed by the
source `stream_index`: register one output stream per inventory row, write each `ReadPacket`
to its mapped index **in the stream's own input timebase**, and rotate on a configured video
stream's **IDR** (`ReadPacket::is_idr`/GP-1), not a generic `is_keyframe()`.

**Timestamps diverge from the program path — deliberately (inv #3).** ISO is an *as-is
archive of the source*, so it **preserves the original input PTS/DTS in each stream's own
input timebase** (`ReadPacket::pts/dts`, `StreamParams.time_base`). It must **NOT** re-stamp
from the output tick counter — `out_pts = f(tick)` is explicitly **rejected for the copy
path** (it collapses B-frame DTS reorder; [ADR-0030](../decisions/ADR-0030.md),
[streaming-gotchas](streaming-gotchas.md): *"for any stream-copy path, clamp
`dts=max(dts,last_dts+1)`"*). A **per-ISO-stream `RestampAccumulator`** is wired into the
segment writer as the **`av_interleaved_write_frame` abort-guard**: it applies a monotonic
DTS/PTS clamp on **every** copied packet and re-anchors the offset at a segment/discontinuity
**boundary** to stitch a continuous timeline across the seam **while preserving each run's
raw inter-packet deltas and B-frame reorder gap**. (Precisely: it is *not* synthesized from
the tick counter — it preserves the source timeline's deltas; the muxer's mechanical
`rescale_ts` into the container timebase preserves the timeline's meaning.)

**Byte-exact on-wire passthrough is DEFERRED.** Dumping raw MPEG-TS/SRT 188-byte packets is
viable only for byte-stream transports and needs a **new raw-AVIO/socket tee** that does not
exist (the only custom AVIO in the tree is `avio_fetch.rs`, read-only text). It is also
**wrong** for RTMP (FLV-tagged messages over a chunk stream), HLS, and RTSP/RTP (elementary
streams fragmented across RTP packets, transport headers discarded on depacketization) —
on-wire bytes are **not 1:1 recoverable** there. Ship **faithful copy-remux** for v1 (the
only path the safe primitives support, and the only correct path for RTMP/HLS/RTSP). Config
surfaces both via `IsoContainer::{ByteExact, Remux}`, with validation **rejecting `ByteExact`
on non-byte-stream transports**.

> **Honest scope of "faithful":** copy-remux preserves **every elementary stream** (stream-
> copy, no re-encode) but is **not byte-faithful** — it re-containers, drops TS null packets,
> original PID assignments, and exact PCR/SCTE-35 transport-layer byte placement. It copies
> the SCTE-35 *stream* (as a data stream per `StreamInventory`) but not its exact on-wire
> placement. Operators needing bit-exact splice forensics must wait for the deferred
> `ByteExact` TS/SRT path.

---

## B. Program (PGM) recording — a rotating file sink in the encode-once fan-out (inv #7)

A PGM recorder is **another mux-only sink consuming the same encoded packets** the live
transports get — **never** a second encode (inv #7). The production wiring is
`crates/multiview-cli/src/pipeline.rs` `consumer_main`: per tick it does
`baker.bake(&item)` → `encoder.encode_frame(frame)` **exactly once** (one `ProgramEncoder`
per run) → `fan_packets(...)` to every registered sink. The existing
`maybe_prepend_program_ts` already prepends a `RunnableOutput::File{ sink:
PacketMuxSink::file("program.ts") }` consuming those **same** fanned packets — *its own
doc-comment says "fed the same one encode, invariant #7."* Program recording **generalizes
that anchor** to be **rotating + retained + arm/disarmable + disk-pressure-gated**: a new
`RunnableOutput::ProgramRecording` carrying a `PacketMuxSink::segment_live` behind the
recorder's own bounded ring.

> **Citation correction (adversarial review).** Do **not** describe the mechanism as
> "consumes the same `Arc<EncodedPacket>`" via `PacketRouter::route`/`EncodeOnceDriver::tick`
> — that **routing-model** path (`fanout.rs`) is **output-crate-internal / test-only**, not
> wired into the engine or CLI. The **production** fan-out is `consumer_main` →
> `encode_frame` once → `fan_packets`, which clones `multiview_ffmpeg::EncodedPacket`
> **per-sink** (`av_packet_ref` + per-sink `av_packet_make_writable`) — *not* one shared
> `Arc<[u8]>`. The per-sink ref-counted copy is **correct** for inv #7 (each muxer rescales
> timestamps in place and must not alias another sink's packet); it is **not** a second
> encode. (The module's own "Two `EncodedPacket` notions" note documents that convergence is
> deferred — RT-13.)

> **Do NOT inherit the `program.ts` coupling verbatim.** `fan_packets` uses `send_bounded`
> over a `sync_channel(4)`, which **blocks the bake consumer for up to `SINK_WEDGE_GRACE`
> (~2 s)** then marks the sink **dead** — it paces the consumer (off the output-clock thread,
> so it doesn't stall the clock) and **cannot auto-resume.** A rotating recorder must
> **interpose its own bounded drop-oldest ring + writer task** (§E) so a wedged disk drops +
> warns + backs off + **auto-resumes**, and never paces the consumer on every frame nor
> permanently marks itself dead.

**The sink.** `PacketMuxSink` holds **no encoder** — it is the packet-fed mux-only egress of
inv #7. `segment_live` rotates on a **video keyframe** (`is_keyframe()` — never a GOP
counter) so every segment is independently decodable, and the muxer's only timestamp job is
the mechanical `av_packet_rescale_ts`; the **Program** packets already carry **tick-derived**
PTS (inv #3 — `ProgramEncoder::encode_frame` stamps `out_pts = tick`, raw input PTS is never
forwarded). `RestampAccumulator` is load-bearing only when a PGM splices across a
discontinuity (encoder restart / copy path).

**Clean vs Dirty (future axis).** The single encoded program is the **dirty** feed because
`bake` burns overlays into pixels *before* `encode_frame` (`StreamBaker::bake` →
`apply_overlays_to_nv12`). v1 records this dirty program at **zero** extra cost. A **clean**
feed is a genuinely **separate encode pipeline** (inv #7 explicitly allows a separate encode
when pixels differ): tap the overlay-free `item.canvas` **before** `bake`, bridge it to a
**second `ProgramEncoder`** under a **distinct `RenditionId`**, behind its own bounded ring.
It **cannot** be achieved by `move_sink`/re-keying — that does **not** encode and the existing
encoder only ever sees the dirty frame. `ProgramFeed::{Dirty, Clean}` in config, `Clean`
noted as future.

---

## C. Segmentation + retention + prune + index (reuse the ADR-0032 segmenter)

Reuse, don't fork, the rolling-window machinery from [ADR-0032](../decisions/ADR-0032.md).
`PacketMuxSink::segment_live` rotates on a video keyframe and publishes each segment via
same-dir `<seg>.tmp` → `rename(2)` (`close_current`, `segment_temp_path` — same-dir, EXDEV-
free). `LivePlaylist` holds a `VecDeque<PathBuf>` window, evicts beyond `window`, and
re-publishes the manifest via `atomic_write` (tmp → `sync_all` → `rename`). `MediaPlaylist`
is the working segment-list-with-time-ranges + monotonic sequence.

**New #1 — DUAL-axis retention (TIME and SIZE).** Today's prune is **count-window only**.
Extend it: after publishing a closed segment, pop the oldest while **any** of
`start_utc < now − retention_time` **OR** `total_bytes > retention_size` **OR**
`count > max_segments` holds, then re-publish the manifest atomically. This is a strict
superset of FFmpeg's `hls_list_size + delete_segments + wrap` (single count axis). **At least
one bound must be set** (validation). **Disk pressure overrides retention** (standard NVR
semantics — §D). **Index:** `MediaPlaylist` for the `.ts`/`.m4s` case; a typed JSON manifest
(atomically published the same way) for the `mkv` ISO-copy case. **Pruning:** immediate
best-effort `unlink` is fine for recordings not served live; if a recording becomes browsable
via a DVR API, route evictions through the (deferred, HLS-2) grace reaper.

**New #2 — segment durability is honest (adversarial review).** The manifest **is** fsync'd
(`live.rs` `atomic_write` → `sync_all` → rename), but the **segment media file is NOT**:
`close_current` → `Muxer::finish` → `write_trailer` (an AVIO buffer flush, *not* `fsync`) →
`rename`, with **no `sync_all` on the segment.** So after a crash/power-loss between rename
and writeback, a segment can be **torn/zero-length under its final name** — the very window a
naive "fsync-before-rename" claim says is closed. Two acceptable resolutions, pick per
config: **(a) document it** — best-effort recorder, segment may be torn after a crash, only
the manifest is durable; or **(b) close the window** — before the `rename` in `close_current`,
`File::open(write_path)?.sync_all()` (mirroring the manifest's barrier) and fsync the
containing directory after rename. This fsync runs on the recorder's **own writer task**
(never the output-clock thread); if the disk is slow it must still degrade to the
drop-oldest/back-off path (§E), never block.

---

## D. Disk-pressure policy (disarm at threshold, resume on recovery)

**Disk sensing must be BUILT — it does not exist** (`rg statvfs|f_bavail|free_space crates/`
→ 0; `availability.rs` is ITU-T G.826 uptime-ratio, not disk). Add an **off-engine** sensor.

- **Where.** `multiview-telemetry` is `#![forbid(unsafe_code)]` ("std::net only"), so it
  **cannot** host the raw `statvfs` call. Put the `unsafe` in a **new dedicated `*sys` crate**
  (e.g. `multiview-fssys`) that relaxes `unsafe_code` `forbid → deny` with one `// SAFETY:`
  block — **exactly** the precedent of `multiview-ntpsys` (wraps `adjtimex`) and
  `multiview-i915pmu` (wraps `perf_event_open`), so consumers stay `forbid(unsafe_code)`.
  `multiview-telemetry` **consumes** its safe API. `libc 0.2.186` is **already** a workspace
  dep (pinned by both `*sys` crates).
- **How.** One `statvfs(2)` syscall, free computed against **`f_bavail`** (blocks available
  to non-root — excludes the ~5 % ext4 root reserve, matching `df`'s *Available*), so a
  `stop_at_pct=80%` threshold is **honest**. `avail = f_bavail.saturating_mul(f_frsize)`,
  `total = f_blocks.saturating_mul(f_frsize)` (checked/saturating — the workspace denies
  `as_conversions`). Zero-init the buffer; on `ret != 0` read `errno`, **warn, and fail SAFE
  → assume pressure → disarm** (a sensor failure must never crash and must not silently keep
  writing). `cfg(unix)`; a Darwin stub/variant verifies `f_frsize` vs `f_bsize`.
- **When.** A **cheap per-segment-OPEN guard** on the writer task (one syscall, no
  allocation, no loop) — **never** on the output-clock thread. Poll on a slow off-engine
  cadence (~1–5 s) otherwise.
- **Policy.** `free < min_free` **OR** `used% ≥ stop_at_pct` → recorder **disarms** (stops
  opening new segments, closes the current one cleanly) + raises `iso/program-disk-pressure`.
  Recover above a **hysteresis band** (`resume_hysteresis_pct`, default 5 %, reusing the
  `degradation::Hysteresis` controller) → **auto-resume.** Disk pressure overrides retention.

---

## E. The bulletproof write path (the cardinal constraint, made structural)

The **only** engine↔recorder coupling is a bounded **drop-oldest** ring + a wait-free
`try_send`; the engine/fan-out **never `.await`s** the recorder — identical posture to the
preview taps and the engine event stream. **The type system does not enforce this** (an
adversarial review found the `PacketSink::deliver` contract is comment-enforced, and the
*production* fan-out blocks-with-timeout), so it is made structural by **construction +
a chaos test**, not by a non-async signature alone.

- **Ingress.** The producer (input read thread for ISO; bake-consumer fan-out for PGM) calls
  a synchronous non-blocking `deliver(&pkt)` → `ring.try_send` → returns. Built on the
  genuinely non-blocking **`BoundedPacketQueue` try-push drop-oldest** pattern (the
  `RtspServerSink::deliver`/`BoundedPacketQueue` precedent), **not** the 2 s-blocking
  `send_bounded` that paces the bake-consumer. Full ring → **drop + count** →
  `recording-dropping` warning. **Never blocks, never grows into RAM.**
- **Egress.** A dedicated **off-hot-path writer task** (own thread/tokio task — **not** the
  engine thread) drains the ring and writes via the safe `Muxer` (mirrors
  `PacketMuxSink::run_av` on its own thread). The engine handed the frame off before the
  recorder is ever fed.
- **State machine (per recorder).** `Disarmed → Armed/Recording → Paused(DiskPressure |
  WriteError | Manual) → Recording`. The engine / program-encode runs through **all**
  transitions; a failing recorder degrades to *Paused/Disarmed with a clear warning*, never
  to *output faltered*, and never to an HTTP error on the engine path.
- **Write-failure → warn + capped back-off.** On `ENOSPC`/`EIO`/`ENODEV`(unmounted)/`EROFS`
  → `Paused(WriteError)` + `recording-write-failing` + **capped-exponential back-off with
  FULL JITTER** (1 s → 2 s → 4 s → … → 30 s cap) via the **`backon`** crate (the resilience
  brief mandates `backon`; `backoff` is **RUSTSEC-2025-0012**; `backon` is **not yet a
  workspace dep** — adding it feature-gated + `cargo deny`-clean, with `cargo deny check` run
  in the same PR to prove the `backoff` path stays closed, is part of this work). Jitter
  prevents N recorders hammering a recovering disk in lockstep. **Keep draining + dropping the
  ring during back-off** — never RAM-buffer, never block. (Note: the *no-panic* guarantee is
  **layered** — the workspace lints ban `unwrap`/`expect`/`panic`/`unreachable`/
  `indexing_slicing`/`as_conversions` in non-test Mosaic code, **plus** the off-thread writer,
  **plus** strict `Result`-propagation of the errno classes; mirror `reconnect.rs`'s
  saturating-integer discipline in the back-off loop. `backon` keeps retrying but does **not**
  itself drain the ring — the writer loop must keep `try_recv`+dropping during back-off.)
- **Auto-resume.** Each retry is a **reopen-and-probe-write** of the target (re-create
  `Muxer`/segment); on success → `Recording`, **clear** the warning. This also recovers an
  unmounted-then-remounted disk.
- **Queue bounds.** **One ring per recorder**, default depth ~256 packets **AND** a byte cap
  (~64 MiB), whichever hits first; the fixed **slot/index** structure is allocated once at arm
  (CLAUDE.md §7 rule 5 — *bounded memory everywhere; queues drop, never grow*; payload bytes
  arrive at runtime and are bounded by the byte cap). `av_packet_ref` clones **share** the
  underlying buffer, so summing `len()` across slots **over-counts** shared bytes and trips
  the cap **early** — conservative, never under-counts. **DROP-OLDEST** for Program (keep
  most-recent; a resumed write restarts near now); **DROP-FORWARD-TO-NEXT-IDR** (whole-GOP,
  with a written discontinuity marker) for ISO stream-copy so a copied file never contains an
  undecodable mid-GOP hole.
  > **RAM ceiling is the ring PLUS libav's interleaver (adversarial review).** A naive
  > "worst-case RAM = depth × max_pkt_bytes" **under-counts**: `write_interleaved`
  > (`av_interleaved_write_frame`) buffers packets internally to reorder by DTS **across
  > streams**; in a multi-stream ISO, a sparse/stalled stream makes libav hold back the other
  > streams' packets in libav-owned memory **outside** the ring's accounting. **Mitigation:**
  > open every ISO/Program muxer via `Muxer::create_with_options` with a bounded
  > `max_interleave_delta` set (the abort-flush knob already supported) so the interleaver is
  > itself bounded. The true ceiling is **ring byte-cap + (max_interleave_delta × max_pkt_bytes
  > × stream_count)** — state that, not the bare formula. The multi-stream RAM chaos test
  > (below) must prove it.
- **Clean bounded teardown (work-schedule #50).** On disarm/shutdown: best-effort,
  **time-boxed** flush — drain to a deadline, `Muxer::finish` (idempotent trailer/moov),
  atomically publish the final manifest, then **join-with-timeout** — **abort + warn** rather
  than hang on a wedged disk. Mirrors `finalize_or_propagate`.
- **Warning transport** is the existing drop-oldest publisher: raise/clear coalesced **by
  code key** over the engine's drop-oldest event publisher (the same lane as `SystemMetrics`),
  surfaced at `GET /api/v1/health` — **NOT** `/livez`/`/readyz` (ADR-R009: a full disk must
  not restart-loop the container).
- **Chaos gate (held-out acceptance, CLAUDE.md §7.2).** Arm **ISO + PGM** against a disk that
  goes **full / unmounts / throws `EIO`** mid-run; assert the output clock keeps emitting one
  valid frame per tick with **zero gap and no RAM growth** while the recorder cycles
  `Recording ↔ Paused` and raises/clears its warnings. **Include the multi-stream ISO RAM
  case** (video + several audio + subtitle + a sparse data/SCTE-35 stream, one stream
  deliberately stalled) and assert **total process RSS stays bounded** — not just the
  single-stream program case the existing guarded path covers. This is what makes
  *ISO = isolated* literally satisfy invariant #10.

---

## F. Config / API / Realtime / UI surface (industry vocabulary)

**Config (additive, serde-default, non-breaking — existing `examples/*.toml` keep parsing).**
Attach recording on the resource it captures — `iso: Option<IsoRecording>` on `Source`
(beside `gpu_pin`/`color_override`), `program: Option<ProgramRecording>` per `Output` variant
with an `Output::program()` accessor mirroring `Output::audio()`/`gpu_pin()`. New types in
`crates/multiview-config/src/recording.rs`:

- `IsoRecording` / `ProgramRecording { armed: bool (default false), location: Option<String>
  (ADR-0032 definable location), segment: SegmentPolicy{duration_seconds}, retention:
  RetentionPolicy, disk: DiskPolicy, container }`,
- `RetentionPolicy { keep_duration: Option<DurationSpec>, keep_size_gib: Option<u64> }`
  (**≥ 1 set**),
- `DiskPolicy { min_free_gib, stop_at_pct: u8, resume_hysteresis_pct }`,
- `IsoContainer` `#[serde(tag="kind")] #[non_exhaustive]` = `Remux{format: mkv|ts}` |
  `ByteExact` (Ts/Srt only, **future**),
- `ProgramFeed` = `Dirty` (v1) | `Clean` (**future**).
- **Never `untagged`** (conventions §5).

**Validation** (in `MultiviewConfig::validate` via `validate_recording`): `ByteExact` only on
byte-stream transports (`Ts`/`Srt`) — **error** on `Rtmp`/`Hls`/`Rtsp` (not 1:1 recoverable);
`stop_at_pct ∈ 1..=99`; `segment.duration_seconds > 0`; **≥ 1 retention bound when armed**;
disk pressure overrides retention.

**API.** Recording *config* is edited through the **existing** `PUT /sources/{id}` and
`/outputs/{id}` (ETag/`If-Match` → `412`, RFC 9457 `problem+json`) — **no new config CRUD**.
New = operational arm/disarm + historic reads in `routes/recordings.rs`, wired into
`api_router`:

- `POST /api/v1/inputs/{id}/recording/arm|disarm` (ISO) and
  `/outputs/{id}/recording/arm|disarm` (Program), via the salvo template (`submit_accepted` →
  `202` + `AcceptedBody{operation_id,kind}`, `Idempotency-Key` honored, **non-blocking
  `try_submit` shed-to-`503`** — inv #10; outcome on the realtime stream). New `Command`
  variants `Arm/Disarm{Iso,Program}Recording`. Arm/disarm is **Class-1 hot** (it only flips a
  best-effort sink's state — strictly safer than `Source.enabled`, which is already hot).
- Reads (computed **off-engine**, role: read): `GET /api/v1/recordings` (list ISO + Program +
  state), `/recordings/{id}/segments` (time-ranged, from the segment manifest),
  `/recordings/{id}/segments/{seg}` + `export?from=&to=`, `/recordings/disk-status` (free
  space per location via the `statvfs` sensor).

**Realtime.** New `Event::RecordingStatus(RecordingStatus{ recorder_id, scope:
Iso{input_id}|Program{output_id}, state: RecorderState, bytes_written, segments_written,
current_segment, disk_free_bytes, oldest_retained, dropped_packets })` with `RecorderState`
`#[non_exhaustive]` = `Recording | Paused | Dropping | Disarmed`. **Re-snapshotable
latest-per-recorder** like `OutputStatus`; carried on the existing Inputs/Outputs topics,
**conflated drop-oldest** (inv #10), **never polled.**

**Warnings (reuse the already-implemented ADR-0035 model — no prerequisite gap).** The
`HealthWarning`/`WarningCode`/store/ingest/`GET /api/v1/health`/`HealthBanner` are **all
implemented and tested.** The recorder only **adds four additive `#[non_exhaustive]`
`WarningCode` variants** (each a one-line enum case + `as_str()` arm + remediation string):

| Code (kebab wire) | Raised when | Carries / remediation |
|---|---|---|
| `iso-disk-pressure` / `program-disk-pressure` | `free < min_free` or `used% ≥ stop_at_pct` | location, free/total; *"free disk space or lower retention / `stop_at_pct`"* |
| `recording-write-failing` | I/O error, backing off | attempt #, next delay, errno class; *"check the recording volume is mounted/writable"* |
| `recording-disarmed` | paused (disk/error), latched | reason; *"resolve disk pressure / write error, then re-arm"* |
| `recording-dropping` | ring overflow | dropped count; *"recording volume too slow — reduce streams or use faster storage"* |

Raise on bad-state entry, **clear** on auto-resume (coalesced by key). **Do NOT** interim-ship
as `Alert` — `Alert` lacks the `code`/`subsystem`/`remediation` fields operators need. Add the
recorder-side emit seam mirroring `emit_capability_warnings`, publishing
`Event::HealthWarningRaised`/`Cleared` off the data plane through the drop-oldest publisher.

**UI.** New `web/src/pages/RecordingsPage.tsx` (+ nav) — per-input **ISO** and per-output
**Program** sections, each with an arm/disarm toggle, a `RecorderState` badge (like
`TileStateBadge`), bytes/segments written, a retention summary, a per-location disk-usage bar,
and a time-ranged segment list/scrubber (`GET /recordings/{id}/segments`). Arm/disarm via the
existing operations client (`operations.ts`/`salvos.ts` mutation pattern); state via
`useEngineEvents` (**no polling**). The **already-existing** `HealthBanner` surfaces the
disk-pressure / write-failing / disarmed / dropping warnings; ensure it is wired into the app
layout + the `SystemPage`. API types via `openapi-typescript` once the `utoipa::path`
annotations land.

---

## 7. Open / deferred axes (explicitly out of v1)

- **Byte-exact on-wire passthrough** (TS/SRT) — needs a raw-AVIO/socket tee (`IsoContainer::ByteExact`).
- **Clean program feed** — a separate encode under a distinct `RenditionId` (`ProgramFeed::Clean`).
- **DVR/browse API with deferred-unlink grace reaper** — for recordings served live (HLS-2).
- **`EncodedPacket` convergence** (RT-13) — the routing-model `Arc<[u8]>` vs production
  `av_packet_ref` copy.
