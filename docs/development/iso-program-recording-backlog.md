# ISO + Program recording — dependency-ordered, PR-sized backlog (REC-0..N)

> Implements [ADR-0037](../decisions/ADR-0037.md) / brief
> [iso-program-recording.md](../research/iso-program-recording.md). Industry terms throughout:
> **ISO** = isolated per-source recording; **Program/PGM** = composited-output recording.
> Each item is one PR. **REC-0 is first and load-bearing**: every recorder inherits the
> guarantee that it can never take out the engine (inv #1/#10), so the bulletproof write path
> ships before any sink that uses it. TDD-first throughout (failing test committed
> separately). Hot-path lints (`unwrap`/`expect`/`panic`/`unreachable`/`indexing_slicing`/
> `as_conversions` denied) apply to all non-test code.

## Dependency graph (text)

```
REC-0 (bulletproof write path)  ── the keystone; everything depends on it
  ├─ REC-1 (backon dep)         ── used by REC-0's back-off  (land alongside/just-before REC-0)
  ├─ REC-2 (Program file sink)  ──► REC-3 (segmentation+retention)
  ├─ REC-4 (from_parameters)    ──► REC-5 (ISO packet-router + IsoMuxSink) ──► REC-3
  ├─ REC-6 (statvfs *sys crate) ──► REC-7 (disk-pressure guard, into REC-0's writer)
  └─ REC-8 (config types+validation) ──► REC-9 (Command + arm/disarm API)
                                          REC-10 (RecordingStatus event)
                                          REC-11 (WarningCode +4 variants + emit seam)
                                          REC-12 (reads API: list/segments/disk-status)
                                                   └─► REC-13 (RecordingsPage + HealthBanner wiring)
                                          REC-14 (chaos/RAM acceptance gate) ── gates the whole feature
```

---

## REC-0 — Bulletproof write path: bounded drop-oldest ring + writer task + back-off state machine
**Crate:** `multiview-output` (e.g. `src/recorder/{ring,writer,state}.rs`). **Depends on:** REC-1.
**This is the keystone — ship first.** The single engine↔recorder coupling.

- A **bounded drop-oldest ring** per recorder: **depth cap (~256) AND byte cap (~64 MiB)**,
  whichever hits first; fixed slot/index allocated **once at arm**. Built on the genuinely
  non-blocking `BoundedPacketQueue` try-push pattern (`rtsp_server/sink.rs` precedent) —
  **never** the 2 s-blocking `send_bounded`. Producer `deliver(&pkt)` → `try_send` → returns;
  full → drop + count. **DROP-OLDEST** policy here; ISO's DROP-FORWARD-TO-NEXT-IDR variant
  lands in REC-5.
- A **dedicated off-hot-path writer task** (own thread/tokio task) drains the ring and writes
  via the safe `Muxer` (mirrors `PacketMuxSink::run_av`).
- A **per-recorder state machine**: `Disarmed → Recording → Paused(DiskPressure|WriteError|
  Manual) → Recording`. On `ENOSPC`/`EIO`/`ENODEV`/`EROFS` → `Paused(WriteError)` +
  **`backon` capped-exponential + full-jitter** back-off (1 s → 30 s cap); **keep draining +
  dropping the ring during back-off** (the writer loop, not `backon`, drains). Each retry is a
  **reopen-and-probe-write** → on success `Recording` + clear. Saturating integer math
  (mirror `reconnect.rs`); never panic.
- **Bounded teardown:** time-boxed flush → `Muxer::finish` (idempotent) → publish final
  manifest → **join-with-timeout** (abort+warn, never hang) — mirror `finalize_or_propagate`.
- Open muxers via `Muxer::create_with_options` with a **bounded `max_interleave_delta`** so
  libav's interleaver can't grow RAM outside the ring.

**Tests:** loom/unit — full ring never blocks producer + RAM flat; injected `EIO` → state
cycles `Recording↔Paused`, back-off bounded with jitter, auto-resumes; teardown joins within
deadline on a wedged writer. **No** `ProgramStatus`/segment/disk logic here — pure transport +
state machine.

## REC-1 — Add `backon` to the workspace (feature-gated, deny-clean)
**Crate:** workspace `Cargo.toml` + `multiview-output`. **Depends on:** none. *(Land with/just before REC-0.)*
- Add `backon` to `[workspace.dependencies]`, feature-gated behind the recording/ffmpeg
  feature. **Not** `backoff` (RUSTSEC-2025-0012). Run `cargo deny check` in the same PR and
  assert the `backoff` transitive path stays absent. Wire it into REC-0's back-off loop.

## REC-2 — Program (PGM) rotating file sink in the encode-once fan-out (inv #7)
**Crate:** `multiview-output` + `multiview-cli/src/pipeline.rs`. **Depends on:** REC-0.
- New `RunnableOutput::ProgramRecording` carrying a `PacketMuxSink::segment_live`, fed the
  **same** single encode (the existing per-sink `av_packet_ref` copy from `fan_packets` —
  **not** a second encode). Interpose REC-0's ring + writer **before** the muxer (do **not**
  reuse `send_bounded`/`SINK_WEDGE_GRACE`).
- v1 = **dirty** program (post-`bake`). `ProgramFeed::Clean` is out of scope (future separate
  encode under a distinct `RenditionId`).

**Tests:** the recorder consumes the same fanned packets, never instantiates an encoder; a
wedged recorder ring does not pace the bake-consumer and does not mark itself permanently dead
(it pauses + auto-resumes).

## REC-3 — Segmentation + dual-axis (time AND size) retention + index (reuse ADR-0032)
**Crate:** `multiview-output` (`hls/{live,media}.rs` + recorder manifest). **Depends on:** REC-2 (and REC-5 for ISO).
- Reuse `segment_live` keyframe rotation + `close_current` `<seg>.tmp → rename` + `LivePlaylist`
  atomic manifest publish. **New:** after publishing a closed segment, prune the oldest while
  **any** of `(start_utc < now − keep_duration)` OR `(total_bytes > keep_size)` OR
  `(count > max_segments)` holds; re-publish atomically. **≥ 1 bound required** (validated in
  REC-8). Disk pressure overrides retention (REC-7).
- **Index:** `MediaPlaylist` for `.ts`/`.m4s`; a typed JSON manifest (atomically published) for
  the `mkv` ISO case + per-segment first-PTS.
- **Segment durability:** either document honestly that only the manifest is fsync-durable, OR
  add `File::open(write_path)?.sync_all()` before the segment `rename` (+ dir fsync) on the
  writer task. State which the config chooses.

**Tests:** prune fires independently on each axis; manifest stays atomically consistent;
keyframe-only rotation (no GOP counter); torn-segment behaviour matches the documented choice.

## REC-4 — `StreamCodecParameters::from_parameters` (copy-stream constructor)
**Crate:** `multiview-ffmpeg` (`packet.rs`). **Depends on:** none. *(Can land in parallel with REC-0..3.)*
- Add `StreamCodecParameters::from_parameters(&codec::Parameters)` — an independent
  `avcodec_parameters_copy` into a fresh owner-less alloc (mirror `from_encoder`), stays
  `Send`, no borrow. Fed by `Demuxer::stream_parameters(index)`. Do **not** extend `StreamKind`.

**Tests:** register a demuxed stream's params via `Muxer::add_stream_from_parameters` and
assert the output stream reproduces `codec_id` + extradata length; assert **no** decoder/encoder
is instantiated.

## REC-5 — ISO capture: all-stream packet-router on the input read thread + `IsoMuxSink`
**Crate:** `multiview-input` (`libav.rs`) + `multiview-output` (`IsoMuxSink`). **Depends on:** REC-0, REC-4.
- Replace `read_packet_for(video_index)` with a **single `read_packet()` loop**: route the
  **video-stream-index** packet to the existing decode/ingest path **unchanged** (unblocked
  primary), and `av_packet_ref`-clone **every** packet into the ISO ring (try_send,
  drop-oldest; **DROP-FORWARD-TO-NEXT-IDR** whole-GOP with a discontinuity marker for the copy
  path). The clone can never delay decode.
- `IsoMuxSink`: register one output stream per `StreamInventory` row (via REC-4) keyed by
  source `stream_index`; write each `ReadPacket` to its mapped index **in the stream's own
  input timebase**. **Preserve input PTS/DTS** (inv #3 — never `f(tick)`). Rotate on a
  configured video stream's **IDR** (`ReadPacket::is_idr`/GP-1). A **per-ISO-stream
  `RestampAccumulator`** is the `av_interleaved_write_frame` monotonic abort-guard across
  segment seams (clamp `dts=max(dts,last_dts+1)`; re-anchor offset at the boundary, preserve
  intra-run deltas).
- Container: `IsoContainer::Remux{mkv|ts}`. `ByteExact` is **deferred** (validated out for
  non-Ts/Srt in REC-8; no raw-AVIO tee in v1).

**Tests:** demux a multi-stream TS (video + audio + subtitle + SCTE-35/data) → every input
index produces an output stream (matching `codec_id`/extradata); video tap behaviour unchanged
vs today; source PTS/DTS preserved; no decoder/encoder on the copy path.

## REC-6 — `statvfs(2)` disk sensor in a dedicated `*sys` crate
**Crate:** new `multiview-fssys` (relax `unsafe_code` forbid→deny + `// SAFETY`). **Depends on:** none.
- Mirror `multiview-ntpsys`/`multiview-i915pmu`. Expose
  `fn statvfs_avail(path) -> Result<DiskSpace{avail_bytes,total_bytes}>` using `libc 0.2.186`'s
  `statvfs`; compute against **`f_bavail`** (`avail = f_bavail.saturating_mul(f_frsize)`),
  checked/saturating arithmetic (no `as`). Zero-init buffer; `ret != 0` → read errno → `Err`.
  `cfg(unix)`; Darwin stub/variant (verify `f_frsize` vs `f_bsize`).

**Tests:** returns plausible free/total for a known path; on a fabricated failure returns
`Err` (caller fails safe). `multiview-telemetry` stays `forbid(unsafe_code)`.

## REC-7 — Disk-pressure guard + telemetry sensor + hysteresis (into REC-0's writer)
**Crate:** `multiview-telemetry` (consume REC-6) + `multiview-output` (writer guard). **Depends on:** REC-0, REC-6.
- A cheap `statvfs` check at each **segment OPEN** on the writer task (never the output-clock
  thread). `free < min_free` OR `used% ≥ stop_at_pct` → `Paused(DiskPressure)`. Auto-resume
  above a hysteresis band (`resume_hysteresis_pct`, default 5 %, reuse `degradation::Hysteresis`).
  Sensor error → **fail safe → disarm**. Disk pressure overrides retention. Slow off-engine
  poll (~1–5 s) for `disk-status`.

**Tests:** crossing the threshold pauses; recovering above hysteresis resumes; flapping is
suppressed; the check never runs on the output-clock thread.

## REC-8 — Config: `iso`/`program` types + validation (additive, serde-default)
**Crate:** `multiview-config` (`recording.rs`, `schema.rs`, `lib.rs::validate`). **Depends on:** none.
- `iso: Option<IsoRecording>` on `Source`; `program: Option<ProgramRecording>` per `Output` +
  `Output::program()` accessor. `IsoRecording`/`ProgramRecording { armed (default false),
  location, segment: SegmentPolicy{duration_seconds}, retention: RetentionPolicy{keep_duration?,
  keep_size_gib?}, disk: DiskPolicy{min_free_gib, stop_at_pct, resume_hysteresis_pct},
  container }`. `IsoContainer` / `ProgramFeed` `#[serde(tag="kind")] #[non_exhaustive]`,
  **never `untagged`**.
- `validate_recording`: `ByteExact` only on `Ts`/`Srt` (error on Rtmp/Hls/Rtsp);
  `stop_at_pct ∈ 1..=99`; `duration_seconds > 0`; **≥ 1 retention bound when armed**.

**Tests:** existing `examples/*.toml` still parse; each validation rule rejects its bad input;
round-trip TOML/JSON.

## REC-9 — Arm/disarm API + `Command` variants (Class-1 hot)
**Crate:** `multiview-control` (`routes/recordings.rs`, `command.rs`). **Depends on:** REC-8, REC-0.
- `POST /api/v1/inputs/{id}/recording/arm|disarm` (ISO) + `/outputs/{id}/recording/arm|disarm`
  (Program), via `submit_accepted` → `202` + `AcceptedBody{operation_id,kind}`,
  `Idempotency-Key`, **non-blocking `try_submit` shed-to-`503`** (inv #10). New `Command`
  variants `Arm/Disarm{Iso,Program}Recording`. **Class-1 hot** (flips a best-effort sink's
  state). Config edits stay on existing `PUT /sources|outputs/{id}` (ETag/`412`, RFC 9457).

**Tests:** arm/disarm returns `202` + op-id; idempotency honored; a saturated command bus sheds
to `503`, never blocks; the engine never awaits the recorder.

## REC-10 — Realtime `Event::RecordingStatus` (re-snapshotable, conflated)
**Crate:** `multiview-events` + `multiview-engine` (publish) + `multiview-control` (snapshot). **Depends on:** REC-0.
- `RecordingStatus{ recorder_id, scope: Iso{input_id}|Program{output_id}, state: RecorderState,
  bytes_written, segments_written, current_segment, disk_free_bytes, oldest_retained,
  dropped_packets }`; `RecorderState` `#[non_exhaustive]` = `Recording|Paused|Dropping|Disarmed`.
  Latest-per-recorder snapshot (like `OutputStatus`); carried on existing Inputs/Outputs
  topics, **conflated drop-oldest** (inv #10), never polled.

**Tests:** snapshot replays latest per recorder; high-rate updates conflate; publishing cannot
back-pressure the engine.

## REC-11 — Four additive `WarningCode` variants + recorder emit seam
**Crate:** `multiview-events` (`event.rs`) + `multiview-output`/`multiview-engine` (emit). **Depends on:** REC-0, REC-7.
- Add `#[non_exhaustive]` `WarningCode` variants: `IsoDiskPressure`/`ProgramDiskPressure`
  (wire `iso/program-disk-pressure`), `RecordingWriteFailing` (`recording-write-failing`),
  `RecordingDisarmed` (`recording-disarmed`), `RecordingDropping` (`recording-dropping`) —
  each one enum case + `as_str()` arm + remediation string. Recorder emit seam mirrors
  `emit_capability_warnings`, publishing `Event::HealthWarningRaised`/`Cleared` off the data
  plane via the drop-oldest publisher; raise on bad-state entry, **clear** on auto-resume
  (coalesced by code key). **Do NOT** use `Alert`. **Reuse** the already-implemented store/
  ingest/`GET /api/v1/health`/`HealthBanner` — no base machinery to build.

**Tests:** each code raises/clears with its remediation; coalesced by key; surfaces at
`GET /api/v1/health` (not `/livez`/`/readyz`).

## REC-12 — Reads API: list / segments / disk-status (off-engine)
**Crate:** `multiview-control` (`routes/recordings.rs`). **Depends on:** REC-3, REC-6, REC-10.
- `GET /api/v1/recordings` (list ISO+Program + state), `/recordings/{id}/segments`
  (time-ranged, from the manifest), `/recordings/{id}/segments/{seg}` + `export?from=&to=`,
  `/recordings/disk-status` (free per location via REC-6). Role: read. Computed **off-engine**.

**Tests:** lists both scopes with state; segment list reflects retention; disk-status reports
honest `f_bavail` free.

## REC-13 — Recordings UI + HealthBanner wiring
**Crate:** `web/` (`src/pages/RecordingsPage.tsx`, nav, layout). **Depends on:** REC-9, REC-10, REC-11, REC-12.
- `RecordingsPage`: per-input ISO + per-output Program sections — arm/disarm toggle,
  `RecorderState` badge (like `TileStateBadge`), bytes/segments, retention summary, per-location
  disk-usage bar, time-ranged segment list/scrubber. Arm/disarm via `operations.ts`/`salvos.ts`;
  state via `useEngineEvents` (**no polling**). Wire the **existing** `HealthBanner` into the
  layout + `SystemPage` (which has no health/disk/recording content today). API types via
  `openapi-typescript` once `utoipa::path` annotations land.

**Tests (Playwright per memory):** arm flips state via the operations client; the badge updates
from a `RecordingStatus` event without polling; the `HealthBanner` renders a disk-pressure
warning + remediation and clears on resume.

## REC-14 — Chaos / RAM acceptance gate (held-out, gates the feature)
**Crate:** held-out acceptance suite (CLAUDE.md §7.2). **Depends on:** REC-0, REC-2, REC-3, REC-5, REC-7.
- Arm **ISO + PGM** against a disk that goes **full / unmounts / throws `EIO`** mid-run. Assert:
  the output clock emits **one valid frame per tick with zero gap**; **total process RSS stays
  bounded** while the recorder cycles `Recording↔Paused` and raises/clears warnings.
- **Multi-stream ISO RAM case:** video + several audio + subtitle + a **sparse/stalled**
  data/SCTE-35 stream; assert RSS bounded with `max_interleave_delta` set (the interleaver
  cannot grow outside the ring). Without this knob the test must fail.
- `cargo deny check` proves the `backoff`/RUSTSEC-2025-0012 path stays closed.

---

### Reflected-out-of-scope (deferred, not in this backlog)
- Byte-exact on-wire TS/SRT passthrough (`IsoContainer::ByteExact` — needs a raw-AVIO tee).
- Clean program feed (`ProgramFeed::Clean` — a separate encode under a distinct `RenditionId`).
- DVR/browse API + deferred-unlink grace reaper (HLS-2).
- `EncodedPacket` convergence (RT-13).
