# Wall-clock + time-of-day sync — PR-sized backlog (SYNC-0..12)

Dependency-ordered, mapped to crates. Reflects **only** what survived adversarial review (every
never-stall / encoded-buffer / reclock-nuance / synced-label mitigation is folded in). See
[wall-clock-sync](../research/wall-clock-sync.md) + [ADR-0038](../decisions/ADR-0038.md).

**TDD-first throughout:** write the failing test first, commit it separately, then implement to green.
Property tests for pure logic. **Default-off ⇒ zero behaviour change** until SYNC-9 wires sync mode.

```
SYNC-0  trust gate (DETECT + Discard=reclock-to-house)   ── gates everything
  ├─ SYNC-1  RTCP-SR + HLS-PDT extraction (Use path map)
  │     └─ SYNC-1b TEMI / SEI-TC / RP188 extraction
  ├─ SYNC-2  encoded-pre-decode delay buffer + pacer wiring
  │     └─ SYNC-3  decoder-D-behind + framestore stays bounded
  │            └─ SYNC-4  sample-by-wall-clock read (T = now − D)
  │                   └─ SYNC-5  MAX→OFFLINE state machine (never stall)
  │                          └─ SYNC-6  output synced-instant LABEL stamping
  │                                 └─ SYNC-7  audio lip-sync under delay
  └─ SYNC-8  PTP tight tier (discipline-not-jam rate slew)
SYNC-9  config + validation (per-source verb, per-program sync, MAX newtype)
SYNC-10 API: Class-2 plan/dry-run + capability-matrix row
SYNC-11 realtime events (WallClockTrust / SyncStatus, drop-oldest)
SYNC-12 UI (badge + toggle + sync indicator + D readout)
```

---

## SYNC-0 — Per-source wall-clock DETECT + TRUST + DISCARD→reclock-to-house *(the trust gate — gates everything)*

**Crates:** `multiview-core` (or `multiview-input::wallclock`), `multiview-input`, `multiview-engine`
(classifier reuse).

- New types (serde internally-tagged, exact `i64` ns / `Rational`, never float):
  `WallClockTier{Trusted,Suspected,None}`, `WallClockOrigin{Ptp,RtcpSr,ProgramDateTime,Temi,DvbTdtTot,
  SmpteSeiTimecode,Rp188Atc,None}`, `WallClockChoice{Use,Discard}`, `WallClockTrust{tier,origin,choice}`,
  `WallClockRef`, `SyncMode{ContentSynced(WallClockRef),HouseClocked}`.
- **Per-origin synthetic-sample adapter** feeding the **existing** `classify_system`/`LockState`
  (`ptp.rs`/`sysref.rs`) — **no new state machine**. Non-PTP origins disable the PTP path-delay guards
  (`delay_outlier_pct`/`step_threshold`) and drive Holdover from staleness; thresholds tuned per tier.
- **Discard/None = the as-built `normalize.rs` path verbatim** (no data-plane code) — anchor first frame
  to house-now, advance by input deltas. Publish `WallClockTrust` via a wait-free latest-slot (inv #10).
- **Tests:** `Locked→Trusted`, `Holdover→Suspected`, `Freerun/Acquiring→None`; bare SEI/RP188 origin
  **caps at Suspected** (never Trusted); two sources with identical embedded TC but differing
  arrival/PTS are **NOT** content-synced; Discard path is bit-identical to today's `normalize.rs`.

## SYNC-1 — RTCP-SR + HLS-PDT extraction (the **Use**-path media→wall map)

**Crates:** `multiview-input` (`webrtc/transport.rs`, `hls.rs`).

- **RTCP SR receiver** (RFC 3550 §6.4.1): add an SR side channel (str0m RTCP callback / libav side
  channel) producing the `(ntp, rtp)` anchor → `WallClockRef` affine map. Today `webrtc/transport.rs`
  surfaces only the bare 32-bit RTP timestamp.
- **HLS media-playlist + PDT scanner** (RFC 8216 §4.3.2.6): today `hls.rs` is master-playlist-only. Add
  a media-playlist scan mapping `PDT(segment) → first-sample PTS`.
- Feed both into SYNC-0's classifier as fresh estimates → can reach **Trusted** (Loose, ms-class).
- **Tests:** SR `(ntp,rtp)` → correct affine `wall(pts)`; PDT→first-sample bind; missing-SR/missing-PDT
  source classifies **None** → reclock-to-house.

### SYNC-1b — TEMI / SMPTE-SEI-TC / RP188 extraction *(additive, after SYNC-1)*

**Crates:** `multiview-input` (`mpegts/`, `st2110/v40.rs`), `multiview-ffmpeg` (`caption_decode.rs`).

- **TEMI** (ISO/IEC 13818-1 adaptation-field af_descriptor) — new parser → affine map.
- **SMPTE ST 12-1 SEI** — mirror the `A53CC` side-data hook with `frame.side_data(S12M_TIMECODE)`
  (confirm `ffmpeg-next` binding `Type::S12M_TIMECODE`). **Suspected at best** (a label, no UTC anchor).
- **RP188/ATC** — decode `st2110/v40.rs` `AncPacket.user_data` → `overlay::timecode::Timecode`; Trusted
  only joined to PTP, else Suspected.
- **Tests:** TEMI affine bind; SEI-TC/RP188 classify **Suspected** even when present; DVB TDT/TOT stays
  service-of-day None (no PTS bind).

## SYNC-2 — Encoded pre-decode alignment buffer + pacer wiring

**Crates:** `multiview-input` (`jitter.rs`/`pacer.rs` + new ingest stage).

- **NET-NEW `ReorderBuffer<EncodedPacket>`** between demuxer and decoder (the existing post-decode
  `ReorderBuffer<ProducedFrame>` is **not** reusable — it holds decoded pixels). Byte-bounded to
  `MAX × peak_bitrate`, **drop-oldest**, released at `deadline = wall_clock(pts) + D` via the pacer
  (**wire the pacer in** — currently unwired).
- **Tests (load-bearing):** `buffer_bytes ≤ MAX × configured_max_bitrate`; **forbid** a
  `ReorderBuffer<DecodedFrame>` at MAX depth; drop-oldest under overflow ("drops, never grows"); deadline
  release ordering.

## SYNC-3 — Decoder runs D-behind + framestore stays bounded

**Crates:** `multiview-input`, `multiview-framestore`.

- Drive the source's decoder `D`-behind real-time off the SYNC-2 buffer so it produces frames only
  around `T`; the framestore ring keeps `RING_CAPACITY = 256` (no enlargement to MAX seconds decoded).
- **Tests:** ring size unchanged under `D`-behind operation; exactly one decoded copy (no second
  materialization); decoder-can't-keep-up → buffer fills → drop-oldest → source goes OFFLINE (handoff to
  SYNC-5), never a memory blow-up.

## SYNC-4 — Sample-by-wall-clock framestore read (`T = our_now − D`)

**Crates:** `multiview-engine` (`drive.rs`).

- **Single seam change:** in sync-mode programs compute `now = tick.pts.saturating_sub(D)` (today
  `let now = tick.pts;`) and pass `T` into `sample_cell`/`read_at`. `D` in integer ns, clamped
  `0 ≤ D ≤ MAX`. **`pts_at`/`deadline_nanos`/cadence byte-for-byte unchanged.**
- **Tests:** cadence-invariance — `pts_at`/`deadline_nanos` byte-identical for `D=0` vs `D>0`; `D` never
  flows into the cadence path; a source with no frame at `T` yields slate, never a stall.

## SYNC-5 — MAX→OFFLINE state machine (never stall)

**Crates:** `multiview-framestore`, `multiview-engine`, `multiview-events`.

- `SyncStatus{Synced,CatchingUp,Offline,HouseClocked}` as a **thin orthogonal overlay** — Offline maps
  onto the **existing** NO_SIGNAL/fail-to-slate ladder; **no new render state** (`SourceState` is
  `#[non_exhaustive]`, untouched). OFFLINE threshold = MAX.
- **Pin `nosignal ≤ MAX`** (config-coherence) so OFFLINE-at-MAX truly fails to slate, not holds a stale
  last-good frame.
- **Tests:** drive `read_at` with `now = T`; a source whose newest frame is `> MAX` behind `T`
  classifies **NO_SIGNAL** (slate) and **never** returns `Fresh`; within-window returns `Fresh`.
  **Chaos/soak:** source pushed `> MAX`-behind → OFFLINE→NO_SIGNAL within one MAX window, output clock
  **never blocks** (inv #1).

## SYNC-6 — Output synced-instant LABEL stamping

**Crates:** `multiview-output` (`hls/media.rs`, SEI injector, NDI/RTP path), `multiview-engine`.

- `label_pts(N) = utc0 + rescale(N, cadence) − D`, subtracted **once at the carrier-write seam**, never
  on the data plane. `D` = **latched, hysteresis-damped per-program constant**, changed only at Class-2
  seams. Apply TAI-UTC offset (37 s) + PTP `currentUtcOffset` before any UTC label, integer ns.
- Carriers: **HLS EXT-X-PROGRAM-DATE-TIME** (add PDT field to `Segment`); **H.264/H.265 SMPTE ST 12-1
  SEI**; **NTP↔RTP** — RTCP SR for true RTP, NDI's own 100 ns timecode for NDI (**distinct** path, not an
  SR). On any `D` change: `EXT-X-DISCONTINUITY` + SEI/SR jam, behind the Class-2 plan — never silent.
- **Tests:** label and data-plane PTS diverge by exactly `D`; `pts_at` byte-identical `D=0` vs `D>0`;
  FreeRun label = our wall-clock; `D`-change steps the label monotonically with a discontinuity, not
  silently; TAI→UTC offset applied for a PTP-derived label.

## SYNC-7 — Audio lip-sync under delay

**Crates:** `multiview-audio`, `multiview-engine`.

- A synced video source delayed by `D` must keep **A/V aligned**: delay the matching audio by the same
  `D` (re-stamp on the same common timeline, exact rationals, never drop/dup per [ADR-T006]). Program-bus
  mix samples each source at `T` consistently with the video read.
- **Tests:** A/V skew within tolerance under a `D`-second video delay; an OFFLINE video source's audio
  also rides the fail path (no orphaned audio); no resample drift (exact rationals).

## SYNC-8 — PTP tight tier (discipline, never jam)

**Crates:** `multiview-engine` (`clock.rs`, `ptp.rs`/`sysref.rs` already built).

- Add a **bounded cadence-rate-scale** input to `OutputClock`; apply the servo's `frequency_ppb` as a
  **rate-only slew** (currently computed-but-unwired). `pts_at` stays pure; **never** step `seed_nanos`
  mid-run. Seed the SMPTE-Epoch origin at start/relock only (mid-run re-jam = Class-2). Keep any
  AES67/RTP send-timestamp computation separate from the tick clock (no second pacer).
- PTP-lost rides Locked→Holdover→Freerun→NTP fallback; label + `D` follow the badge.
- **Tests:** bounded per-update slew + accumulated-deviation clamp; **adversarial PtpSample soak**
  (grandmaster step, outliers, loss→holdover→freerun) → `out_pts` strict-monotonic + cadence in
  tolerance throughout; no mid-run seed mutation on a routine slew.

## SYNC-9 — Config + validation

**Crates:** `multiview-config` (`schema.rs`, `program.rs`).

- Per-source `wall_clock: Option<SourceWallClock{choice: Use|Discard}>` on `Source` (internally-tagged,
  beside `auth`/`color_override`/`captions`/`gpu_pin`). Per-program `sync:{mode: FreeRun(default)|
  WallClockSync, max_window: <validated tier-bounded Duration newtype>, tier: Loose|Tight}` on
  `ProgramSpec` (semantics on `ProgramKind::Multiview`); legacy flat path desugars onto the synthesized
  `main` program. **Default FreeRun = zero behaviour change.**
- **Validation:** (1) reject a single-cell `Multiview` (+ `Passthrough` when that kind lands) with
  `WallClockSync`; (2) bound `max_window` within the tier MAX (Loose ~10 s, Tight ~0.5–1 s, hard cap
  ≤ 30 s); (3) flag (warning, runtime) a `Used` source whose detected confidence can only be None/house.
- **Tests:** round-trip TOML+JSON; rejection cases; MAX newtype rejects out-of-tier and `>` hard cap;
  default config unchanged.

## SYNC-10 — API: Class-2 plan/dry-run + capability-matrix row

**Crates:** `multiview-control` (`routes/sources.rs`, `routes/config.rs`, `routing.rs`).

- Per-source Use/Discard rides generic source CRUD (ETag/If-Match, RBAC, audit). Per-program sync/
  `max_window` change = **Class-2/Reset-lite** (inv #11) through the **existing** `RouteClass` classifier
  — mandatory plan/dry-run surfacing "will reset N outputs / consumers reconnect" **before** apply. Add a
  `Program.sync.{mode,max_window,tier}` capability-matrix row.
- **Tests:** sync change classifies Class-2 and returns `202` + operation id; dry-run reports impacted
  outputs; per-source verb change applies hot.

## SYNC-11 — Realtime events (drop-oldest, never back-pressure)

**Crates:** `multiview-events` (`event.rs`), `multiview-control`, web TS types.

- `WallClockTrust` (tier + origin + Used|Discarded→house) on the **Inputs** topic (extend
  `InputConnection`); `SyncStatus` on the **Tiles** topic, orthogonal to `LifecycleState`, **Offline→
  NO_SIGNAL**. Conflated, **drop-oldest** broadcast; engine publish is non-blocking (inv #10). Register in
  openapi/asyncapi + generated TS.
- **Tests:** events conflate under load; a slow consumer cannot back-pressure the engine (chaos gate);
  Offline maps onto NO_SIGNAL.

## SYNC-12 — UI

**Crates:** `web/` (React 19, generated OpenAPI client — no hand-written types).

- (1) per-source trust **badge** (Trusted/Suspected/None) + Use/Discard **toggle** by `SourcePalette`;
  (2) per-program sync toggle + MAX + tier with the **Class-2 confirm**; (3) per-tile sync indicator
  (Synced/Catching-up/Offline/House + skew) extending `TileStateBadge.tsx` on the orthogonal axis —
  TC-only sources show *"TC present, phase unknown"*, never "synced"; (4) program rollup
  "N/M synced, K offline" + the added-latency **`D`** readout on the `SystemPage` program card.
- All best-effort, tolerate dropped/conflated events. TS fields may ship ahead of AsyncAPI regen via the
  `envelope.ts` `TileStateDeltaData` precedent, then folded via `cargo xtask gen-openapi` +
  `npm run generate:events`.
- **Tests:** Playwright e2e — badge reflects tier; Class-2 confirm gates a sync toggle; Offline tile
  shows slate + Offline reason; `D` readout updates. (Per repo guidance, drive a real browser.)
