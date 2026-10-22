# Feature-Request Intake & Dependency Catalog — 2026-06-13

**Status:** Design intake (Proposed) — **docs-only**; no code shipped in this pass.
**Scope:** a large operator feature-request batch, researched / validated / cross-linked and turned
into house-style **research briefs + ADRs**. Implementation follows later in dependency-ordered waves
under each brief's own plan + backlog lane.
**Base:** authored on a clean worktree off `origin/main` (the live working checkout was a stale,
mid-reconcile detached HEAD 169 commits behind main). Branch `docs/feature-intake-2026-06-13`.
**Cross-vendor review:** each brief + its ADRs went through a fresh-context **OpenAI Codex** adversarial
review (the alternate vendor; Claude authored → Codex reviews, per
[agent-guardrails §C](agent-guardrails.md)). A human is the final approver — AI review is never the gate.

> This is the **spine**: every operator request below maps to its classification, the doc(s) that now
> cover it, the ADR(s) it drives, and its dependencies. Where a request was **already designed** on
> current main (the production-switcher and Conspect programs), it is linked — not duplicated.

---

## 1. How to read the matrix

- **COVERED** — already designed on current main; this intake only **links** to it (no new doc).
- **EXTEND** — an existing brief/ADR is extended by a new doc (the new doc says exactly what it adds).
- **NEW** — no prior coverage; a new brief + ADR(s).
- **DEFECT** — an observed runtime defect; captured as **diagnosis + a hardware-verification plan** in
  [`current-defects-2026-06-13.md`](current-defects-2026-06-13.md) (not a code fix this pass).

Invariant discipline is asserted in every brief: **#1** (the output clock is untouchable — nothing
may block/de-pace program output; inputs/cues/detection/offsets are *sampled*) and **#10** (control /
preview / aux / detection / cues / logging never back-pressure the engine). Everything networking is
**IPv6-first**; anything proprietary is **vendor-neutral / clean-room / nominative** (the ZowieTek-driver
posture). ADRs are **Proposed**.

## 2. The intake matrix

| # | Operator request | Class | Primary doc | ADR(s) | Key dependencies / links |
|---|---|---|---|---|---|
| R1 | HLS program output "bursting" (only HLS checked) | DEFECT | [current-defects](current-defects-2026-06-13.md) §HLS-bursting | — | [hls-delivery](../research/hls-delivery.md), [ADR-T005](../decisions/ADR-T005.md), [streaming-gotchas §4](../research/streaming-gotchas.md) |
| R2 | No audio on program output | DEFECT | [current-defects](current-defects-2026-06-13.md) | — | [ADR-R005](../decisions/ADR-R005.md), [ADR-0059](../decisions/ADR-0059.md), [switcher-audio](../research/switcher-audio.md) |
| R3 | Tracks selectable, not a free-text list | EXTEND | [webui-operability-gaps](../research/webui-operability-gaps.md) | [ADR-0093](../decisions/ADR-0093.md) | [ADR-M004](../decisions/ADR-M004.md), [ADR-0036](../decisions/ADR-0036.md), [decoupled-routing](../research/decoupled-routing.md) (StreamInventory); also [current-defects](current-defects-2026-06-13.md) |
| R4 | Layout editor can enable/disable subtitles | EXTEND | [webui-operability-gaps](../research/webui-operability-gaps.md) | [ADR-0094](../decisions/ADR-0094.md) | [ADR-R007](../decisions/ADR-R007.md), [ADR-0019](../decisions/ADR-0019.md), [ADR-W004](../decisions/ADR-W004.md), RT-10a subtitle breakaway |
| R5 | Layout orientation → program + preview (VLC orients) | NEW | [output-metadata-and-orientation](../research/output-metadata-and-orientation.md) | [ADR-0089](../decisions/ADR-0089.md) | [display-out](../research/display-out.md), [preview-subsystem](../research/preview-subsystem.md), [ADR-M010](../decisions/ADR-M010.md) |
| R6 | Full output metadata (where transport/codec support) | EXTEND | [output-metadata-and-orientation](../research/output-metadata-and-orientation.md) | [ADR-0088](../decisions/ADR-0088.md) | [ADR-M002](../decisions/ADR-M002.md), [ADR-C006](../decisions/ADR-C006.md) |
| R7 | Preview anywhere a stream is referenced, non-disruptive | EXTEND | [webui-operability-gaps](../research/webui-operability-gaps.md) | [ADR-0090](../decisions/ADR-0090.md) | [preview-subsystem](../research/preview-subsystem.md), [ADR-P002](../decisions/ADR-P002.md)/[P003](../decisions/ADR-P003.md) |
| R8 | Inputs show stale/no-signal when signal present | DEFECT | [current-defects](current-defects-2026-06-13.md) §false-stale | — | [resilience-and-av](../research/resilience-and-av.md), [ADR-R001](../decisions/ADR-R001.md), [ADR-T002](../decisions/ADR-T002.md) |
| R9 | Inputs show no-audio when audio present | DEFECT | [current-defects](current-defects-2026-06-13.md) | — | [ADR-R005](../decisions/ADR-R005.md), [ADR-R006](../decisions/ADR-R006.md) |
| R10 | Full ONVIF incl PTZ/time/codecs/res; discovery; manual cross-subnet add; outputs-as-ONVIF; PTZ passthrough | NEW | [onvif-and-ptz](../research/onvif-and-ptz.md) | [ADR-0062](../decisions/ADR-0062.md)/[0063](../decisions/ADR-0063.md)/[0064](../decisions/ADR-0064.md) | [managed-devices](../research/managed-devices.md), [ADR-M008](../decisions/ADR-M008.md)/[M009](../decisions/ADR-M009.md), [ADR-0006](../decisions/ADR-0006.md) RTSP |
| R11 | Full UniFi Protect NVR compatibility for outputs | NEW | [unifi-protect-compat](../research/unifi-protect-compat.md) | [ADR-0065](../decisions/ADR-0065.md) | [onvif-and-ptz](../research/onvif-and-ptz.md), [ADR-0006](../decisions/ADR-0006.md) (clean-room/nominative) |
| R12 | Prep for future image/video models (object detection) | NEW | [object-detection-ai](../research/object-detection-ai.md) | [ADR-0066](../decisions/ADR-0066.md) | [preview-subsystem](../research/preview-subsystem.md) (frame tap), [self-aware-placement](../research/self-aware-placement.md) |
| R13 | Finely configurable motion / scene-change detection | NEW | [motion-scene-detection](../research/motion-scene-detection.md) | [ADR-0068](../decisions/ADR-0068.md) | [object-detection-ai](../research/object-detection-ai.md) (shared tap), [efficiency](../research/efficiency.md) |
| R14 | Record on named object detection | NEW | [object-detection-ai](../research/object-detection-ai.md) | [ADR-0067](../decisions/ADR-0067.md) | [recording-storage-offload](../research/recording-storage-offload.md), [realtime-api](../research/realtime-api.md) |
| R15 | Record on motion / scene change | NEW | [motion-scene-detection](../research/motion-scene-detection.md) | [ADR-0068](../decisions/ADR-0068.md) | [recording-storage-offload](../research/recording-storage-offload.md) |
| R16 | Offload ISOs/recordings to S3 | NEW | [recording-storage-offload](../research/recording-storage-offload.md) | [ADR-0070](../decisions/ADR-0070.md) | [iso-program-recording](../research/iso-program-recording.md), [ADR-0037](../decisions/ADR-0037.md) |
| R17 | S3 Tables (Iceberg) via SeaweedFS for metadata/event data | NEW | [recording-storage-offload](../research/recording-storage-offload.md) | [ADR-0071](../decisions/ADR-0071.md) | [ADR-0070](../decisions/ADR-0070.md), [ADR-0052](../decisions/ADR-0052.md) (consent/retention) |
| R18 | PTZ — is there a standard? | NEW (answered) | [onvif-and-ptz](../research/onvif-and-ptz.md) §PTZ | [ADR-0064](../decisions/ADR-0064.md) | ONVIF PTZ + VISCA / VISCA-over-IP |
| R19 | RM-LP350G MIDI + future MIDI control surfaces | NEW | [control-surfaces-midi](../research/control-surfaces-midi.md) | [ADR-0086](../decisions/ADR-0086.md) | [ADR-W021](../decisions/ADR-W021.md), [ADR-M012](../decisions/ADR-M012.md), [switcher-audio](../research/switcher-audio.md), [managed-devices](../research/managed-devices.md) |
| R20 | Native YouTube/Vimeo URL (iso record, download-to-VT, S3/Iceberg) | EXTEND | [online-services-input](../research/online-services-input.md) | [ADR-0087](../decisions/ADR-0087.md) | [ADR-0015](../decisions/ADR-0015.md) (yt-dlp), [media-playout](../research/media-playout.md), [recording-storage-offload](../research/recording-storage-offload.md) |
| R21 | Live changes everywhere; high-risk "write-don't-apply" gate + pending bar | EXTEND | [webui-operability-gaps](../research/webui-operability-gaps.md) | [ADR-0091](../decisions/ADR-0091.md) | [ADR-M005](../decisions/ADR-M005.md), [ADR-M012](../decisions/ADR-M012.md), [ADR-W018](../decisions/ADR-W018.md)/[W022](../decisions/ADR-W022.md), [ADR-R004](../decisions/ADR-R004.md)/[R010](../decisions/ADR-R010.md) |
| R22 | Full SIP/TLS in/outbound calling (audio + video) | NEW | [sip-calling](../research/sip-calling.md) | [ADR-0084](../decisions/ADR-0084.md)/[0085](../decisions/ADR-0085.md) | [webrtc](../research/webrtc.md), [ADR-0048](../decisions/ADR-0048.md)/[0049](../decisions/ADR-0049.md), [switcher-audio](../research/switcher-audio.md) (mix-minus return) |
| R23 | Audio bus / audio routing | EXTEND | [switcher-audio](../research/switcher-audio.md) | [ADR-0077](../decisions/ADR-0077.md)/[0078](../decisions/ADR-0078.md)/[0079](../decisions/ADR-0079.md) | **extends** [ADR-0059](../decisions/ADR-0059.md) + [production-switcher §8](../research/production-switcher.md) (the first slice) |
| R24 | Video bus / video routing | **COVERED** | [production-switcher](../research/production-switcher.md) §4–§6 | [ADR-0054](../decisions/ADR-0054.md)–[0056](../decisions/ADR-0056.md) | + [decoupled-routing](../research/decoupled-routing.md)/[ADR-0034](../decisions/ADR-0034.md) crosspoints |
| R25 | ISO recording (raw) | COVERED/EXTEND | [iso-program-recording](../research/iso-program-recording.md) | [ADR-0037](../decisions/ADR-0037.md) | extended by [recording-storage-offload](../research/recording-storage-offload.md) |
| R26 | ISO recording (transcoded) | NEW | [recording-storage-offload](../research/recording-storage-offload.md) | [ADR-0069](../decisions/ADR-0069.md) | extends [ADR-0037](../decisions/ADR-0037.md) |
| R27 | S3 storage | NEW | [recording-storage-offload](../research/recording-storage-offload.md) | [ADR-0070](../decisions/ADR-0070.md) | — |
| R28 | GPI/GPO/GPIO/relay (Pi GPIO mode): edges/levels/min-pulse/debounce/retrigger-lockout | NEW | [broadcast-cues](../research/broadcast-cues.md) | [ADR-0073](../decisions/ADR-0073.md) | [display-out](../research/display-out.md) (node tier), [managed-devices](../research/managed-devices.md) |
| R29 | SCTE-104 (TCP or SDI VANC) | NEW | [broadcast-cues](../research/broadcast-cues.md) | [ADR-0074](../decisions/ADR-0074.md) | SMPTE ST 2010 (VANC) |
| R30 | SCTE-35 | EXTEND | [broadcast-cues](../research/broadcast-cues.md) | [ADR-0074](../decisions/ADR-0074.md) | [ADR-0034](../decisions/ADR-0034.md) (already crosspoints SCTE-35→output) — adds ingest+emit |
| R31 | HLS/DASH manifest cue (EXT-X-DATERANGE, CUE-OUT/IN, DASH events) | NEW | [broadcast-cues](../research/broadcast-cues.md) | [ADR-0075](../decisions/ADR-0075.md) | [hls-delivery](../research/hls-delivery.md) |
| R32 | BXF / automation metadata | NEW | [broadcast-cues](../research/broadcast-cues.md) | [ADR-0076](../decisions/ADR-0076.md) | SMPTE ST 2021 |
| R33 | Cue-action vocabulary (macro/cut/auto/VT/lower-third/aux/fade/mute/record/emit/GPO/WS/webhook) | EXTEND/COVERED | [broadcast-cues](../research/broadcast-cues.md) | [ADR-0072](../decisions/ADR-0072.md) | **maps onto** [ADR-M012](../decisions/ADR-M012.md)/[W021](../decisions/ADR-W021.md) macros, [switcher-audio](../research/switcher-audio.md), [recording-storage-offload](../research/recording-storage-offload.md) |
| R34 | Motion graphics as HTML/CSS(+JS): templating, n-source, render-to-VT | NEW | [motion-graphics-html](../research/motion-graphics-html.md) | [ADR-0080](../decisions/ADR-0080.md)/[0081](../decisions/ADR-0081.md)/[0082](../decisions/ADR-0082.md) | [ADR-R008](../decisions/ADR-R008.md)/[0016](../decisions/ADR-0016.md) overlay, [media-playout](../research/media-playout.md), [ADR-0058](../decisions/ADR-0058.md) alpha |
| R35 | URL as input (JS on load/event, refresh, conditional) | NEW | [url-input](../research/url-input.md) | [ADR-0083](../decisions/ADR-0083.md) | shares the [motion-graphics-html](../research/motion-graphics-html.md) engine ([ADR-0080](../decisions/ADR-0080.md)) |
| R36 | Refire hardware assessment + allocation; confirm it may interrupt everything | EXTEND | [webui-operability-gaps](../research/webui-operability-gaps.md) | [ADR-0092](../decisions/ADR-0092.md) | [self-aware-placement](../research/self-aware-placement.md), [ADR-0035](../decisions/ADR-0035.md)/[0018](../decisions/ADR-0018.md) |
| R37 | System stats: our load vs everyone else (encoder owner, why CPU pegged) | EXTEND | [system-stats-attribution](../research/system-stats-attribution.md) | [ADR-0061](../decisions/ADR-0061.md) | [gpu-monitoring-and-scheduling](../research/gpu-monitoring-and-scheduling.md), [self-aware-placement](../research/self-aware-placement.md) |
| R38 | Logging must be source/output/layout specific | EXTEND | [observability-logging](../research/observability-logging.md) | [ADR-0060](../decisions/ADR-0060.md) | [ADR-MV001](../decisions/ADR-MV001.md); [current-defects §logging](current-defects-2026-06-13.md) |
| R39 | Input offsets — audio + video, muxed or separate feeds | NEW | [input-and-consumption-offsets](../research/input-and-consumption-offsets.md) | [ADR-T017](../decisions/ADR-T017.md) | [ADR-T008](../decisions/ADR-T008.md), [ADR-0038](../decisions/ADR-0038.md), [ADR-0059](../decisions/ADR-0059.md) |
| R40 | Offset levels — universal input / per-output / per-layout / per-switcher | NEW | [input-and-consumption-offsets](../research/input-and-consumption-offsets.md) | [ADR-T016](../decisions/ADR-T016.md) | [ADR-0034](../decisions/ADR-0034.md) crosspoints, [ADR-0030](../decisions/ADR-0030.md), [ADR-M005](../decisions/ADR-M005.md) apply-class |

## 3. New documents created in this intake

**Research briefs (`docs/research/`):** observability-logging · system-stats-attribution ·
onvif-and-ptz · unifi-protect-compat · object-detection-ai · motion-scene-detection ·
recording-storage-offload · broadcast-cues · switcher-audio · motion-graphics-html · url-input ·
sip-calling · control-surfaces-midi · online-services-input · output-metadata-and-orientation ·
webui-operability-gaps · input-and-consumption-offsets. **(17)**

**Triage (`docs/development/`):** [current-defects-2026-06-13.md](current-defects-2026-06-13.md).

**ADRs (`docs/decisions/`):** **0060–0094** (35) + **0077–0079** switcher-audio (within that range) +
**T016, T017** (input/consumption offsets — T-series, alongside the timing group). **(37 total.)**

## 4. Already covered on current main (linked, not duplicated)

- **Production-switcher program** ([production-switcher.md](../research/production-switcher.md) +
  [media-playout.md](../research/media-playout.md), ADR-0054–0059 + C007/M012/MV006/P007/R011/RT008/T015/
  W021–W023) already designs **video bus / M-E / transitions / keyers / FTB / multi-box (R24)**, **VT
  load/play/stop + stills + alpha media**, **macros + memories + salvo (R33 actions)**, **cut/auto/take**,
  **lower-third = DSK**, **tally/TSL**, and the **switcher-audio first slice (ADR-0059)** that R23
  extends.
- **Conspect program** (ADR-0050–0053, [conspect-account-architecture.md](../research/conspect-account-architecture.md))
  — account/licensing/mesh/two-pipe-telemetry-consent/support; the consent/retention boundary that
  R17 (Iceberg event data) and R38 (logging) respect.
- **Decoupled routing** ([ADR-0034](../decisions/ADR-0034.md)) already crosspoints **SCTE-35→output**
  and **audio/subtitle breakaway** (the basis R30/R4 extend).

## 5. Backlog lanes (dependency-aware fanout — entry points)

Each brief carries its own implementation plan + honest open questions; these are the **lane entry
points** for the eventual build, ordered by dependency. Lanes are independent unless a "depends-on"
is named. (No code is started this pass.)

| Lane | Area | First slices (entry points) | Depends on |
|---|---|---|---|
| **LOG** | observability-logging | resource-scoped span hierarchy → libav AVClass→resource correlation → log-tail API | — |
| **STATS** | system-stats-attribution | per-process NVML/NVENC + /proc/cgroup + fdinfo probe → ours-vs-co-tenant model → stats page | self-aware-placement |
| **OFFSET** | input-and-consumption-offsets | AudioReader-cursor audio offset → small decoded-ring video offset → encoded-delay large offset → 4-level resolution + apply-class | ADR-0038, ADR-0059 §5 |
| **ONVIF** | onvif-and-ptz | WS-Discovery + manual add → ONVIF client (Device/Media/Imaging/PTZ) → ONVIF server (outputs-as-camera) → PTZ (ONVIF+VISCA) + passthrough | managed-devices (M008/M009) |
| **UNIFI** | unifi-protect-compat | confirm UniFi ONVIF/RTSP adoption path → outputs-as-camera profile → clean-room adoption shim | ONVIF (0063) |
| **DET** | object-detection-ai | read-only frame-tap seam → detector provider (ONNX/external) → detection events → record-on-object | preview-tap, REC |
| **MOTION** | motion-scene-detection | luma-diff/scene/black-freeze on the shared tap → zones/masks/debounce → record-on-motion | DET (shared tap) |
| **REC/S3** | recording-storage-offload | transcoded ISO + trigger-gating → S3 multipart offload (bounded) → Iceberg/SeaweedFS event catalog | iso-program-recording (0037), CUES/DET/MOTION (triggers) |
| **CUES** | broadcast-cues | cue bus + action vocabulary (map to switcher) → GPIO node → SCTE-104/35 ingest+emit → HLS/DASH manifest cues → BXF | production-switcher (M012/W021), decoupled-routing |
| **SWAUD** | switcher-audio | strips+buses+send-matrix (ext ADR-0059) → mix-minus/IFB/talkback-safety → monitor/PFL/AFL + AFV-graph + output routing | ADR-0059 first slice |
| **MGFX** | motion-graphics-html | headless GPU-surface engine (isolated) → data-binding/templating + n-source → render-to-VT | overlay (R008/0016), media-playout, URLIN (shared engine) |
| **URLIN** | url-input | web-page SourceKind on the MGFX engine → JS-on-load/event → interval + conditional refresh | MGFX engine (0080) |
| **SIP** | sip-calling | SIP/SIPS+SRTP stack → reuse WebRTC media engine → mix-minus return + tile video | webrtc (0048/0049), SWAUD (mix-minus) |
| **SURF** | control-surfaces-midi | generic surface model → MIDI (midir) binding → map to switcher/macros/faders → RM-LP350G profile | production-switcher (W021/M012), SWAUD |
| **OSVC** | online-services-input | resolver ext (Vimeo) → download-to-VT → record-to-S3/Iceberg | ADR-0015, media-playout, REC/S3 |
| **OUTMETA** | output-metadata-and-orientation | per-transport metadata model (ext M002/C006) → orientation (canvas vs display-tag) → flow to program+preview | display-out, preview |
| **WEBUI** | webui-operability-gaps | ubiquitous preview → live-apply audit + staging + high-risk gate → re-assess-hw confirm → typed track select → subtitle toggle | preview, M005/M012, self-aware-placement |
| **DEFECT** | current-defects (triage) | verify-on-hardware: HLS bursting · program audio · false stale/no-signal · false no-audio · logging | LOG (logging gap blocks precise triage) |

## 6. Consolidated operator decision points (from the briefs' open questions)

These are the genuine "operator's call" items surfaced honestly across the briefs (each brief has the
full list):

1. **UniFi Protect** ([unifi-protect-compat](../research/unifi-protect-compat.md)) — stance on any
   proprietary adoption protocol beyond the open ONVIF/RTSP path (clean-room boundary).
2. **RM-LP350G** ([control-surfaces-midi](../research/control-surfaces-midi.md)) — confirm the device
   protocol (MIDI assumed); the generic surface + profile slot ships regardless.
3. **Object-detection seam** ([object-detection-ai](../research/object-detection-ai.md)) — embedded
   ONNX vs external model service as the first detector (the seam supports both).
4. **HTML-graphics engine** ([motion-graphics-html](../research/motion-graphics-html.md)) — which
   headless engine (license + efficiency weight); it is an opt-in feature regardless.
5. **Offsets** ([input-and-consumption-offsets](../research/input-and-consumption-offsets.md)) —
   replace-vs-additive override semantics (pinned replace); the small-video-ring depth bound (needs
   hardware measurement); negative-offset cap UX.
6. **Switcher audio** ([switcher-audio](../research/switcher-audio.md)) — aux-bus count for the first
   UI; Program loudness on-by-default vs metering+limiter; which external audio devices are MVP.
7. **S3/Iceberg** ([recording-storage-offload](../research/recording-storage-offload.md)) — confirm
   SeaweedFS S3-Tables/Iceberg maturity for the event-catalog sidecar.
8. **Online services** ([online-services-input](../research/online-services-input.md)) — ToS/legal
   posture for Vimeo/YouTube ingest + download.

## 7. Cross-vendor adversarial review

Per [agent-guardrails §C](agent-guardrails.md), each brief + its ADRs received a **fresh-context
OpenAI Codex (GPT-5)** review against a design-doc checklist (spec conformance · external-standard
accuracy · invariant #1/#10 · no-contradiction-of-shipped-ADRs · vendor-neutrality/clean-room ·
honesty · link/numbering integrity). Findings were triaged and critical/correctness/vendor-neutrality
items fixed before this catalog was finalized. **A human is the final approver** — AI review is never
the merge gate.
