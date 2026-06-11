# Roadmap

Multiview's design is **complete and pinned** (architecture, API/UI, 89 ADRs, 10 verification-hardened
research briefs); implementation proceeds in the phased milestones below. This is the top-level
overview — the detailed per-milestone plan with exit criteria and tasks is in
[`docs/roadmap.md`](docs/roadmap.md), the per-feature status is in [`FEATURES.md`](FEATURES.md), and
the management-completeness gate is the [212-item checklist](docs/development/completeness-checklist.md).

**Status legend:** ✅ Done · 🔵 In progress · 📋 Planned

| Milestone | Focus | Exit criteria (short) | Status |
|-----------|-------|------------------------|--------|
| **M0** — Scaffold & CI | Repo, workspace, CI, docs | Workspace compiles; CI green (fmt/clippy/test/deny/inclusive-lang); docs + 89 ADRs published | ✅ Done |
| **M1** — First pixels | Single source: decode → compose (software) → output | One input decoded, composited (software), served to file/RTSP, driven by the fixed-cadence **output clock** | 🔵 In progress |
| **M2** — GPU path | GPU compositor + HW decode/encode + HAL negotiation | wgpu/Metal/CUDA compositor; NVDEC/NVENC, VideoToolbox, VAAPI/QSV; per-stage negotiation; NV12-throughout; zero-copy within a vendor island | 🔵 In progress |
| **M3** — Bulletproof | Multi-source + output-clock + framestore + resilience | N inputs; last-good-frame stores + tile state machine; supervised reconnect; **never-falters** output-validity SLO holds under fault injection | 🔵 In progress |
| **M4** — Full I/O + audio | HLS/LL-HLS/NDI/SRT + multistream audio | All input/output protocols; discrete per-input audio tracks + mixed program bus; correct A/V sync | 🔵 In progress |
| **M5** — A/V features + color | Subtitles, overlays & color correctness | libass burn-in + passthrough; overlay layers (labels/clocks/meters/alert cards); the 4-axis color pipeline with output tagging + ffprobe verify | 🔵 In progress |
| **M6** — Control API | Web API + OpenAPI | axum `/api/v1`, OpenAPI 3.1 + Scalar, RFC 9457 errors, auth/RBAC, SQLite, the engine command bus | 🔵 In progress |
| **M7** — Management UX | Web app + preview + realtime | React app + drag-and-drop layout editor; WS/SSE + AsyncAPI; isolated preview (input/program/output) | 🔵 In progress |
| **M8** — Efficiency | Efficiency, adaptive control & density | Decode-at-display-resolution; admission control + ordered degradation ladder; published density numbers per commodity tier | 🔵 In progress |
| **M9** — Hardening | Chaos, soak & release | Chaos/fault-injection + multi-day soak; mutation-score gate; packaging (container + macOS native); first release | 📋 Planned |
| **M10** — Monitoring & alarm engine | Content-aware fault/QC probes (black/freeze with zone+dwell, audio silence/over/clip/phase/imbalance, loudness violation + dialnorm mismatch, caption presence-loss, format/AFD/colorimetry mismatch, optional open-metric QoE, ETSI TR 101… | Each probe is configurable (threshold/zone/dwell) and verifiable under synthetic fault injection; alarms carry X.733 severity with dwell/hysteresis, latch, ack and probe->tile->group->system roll-up, exposed over REST/WebSocket; SNMP… | 📋 Planned |
| **M11** — UMD, tally & broadcast control surface | Dynamic labels, tally and operator control: TSL UMD v3.1/v4.0/v5.0 listener+sender (configurable port; v5.0 over UDP primary; DLE/STX only on TCP; ASCII/UTF-16LE; 0xFFFF broadcast) driving label + multi-region (LH/RH/text) tally with… | TSL v3.1/4.0/5.0 round-trips with at least one third-party controller; tiles tally red/green/amber from an external bus via the arbiter; UMD text updates live without layout reload; virtual GPI/GPO + IS-07 trigger and emit; salvos apply… | 📋 Planned |
| **M12** — IP-broadcast I/O, multi-head walls & router control | Professional IP-facility tier: native SMPTE ST 2110-20/-30/-40 ingest+egress, ST 2110-22 (JPEG XS), ST 2022-6, ST 2022-7 hitless dual-path (in+out), PTP/ST 2059-2 timing + per-input frame-sync; AMWA NMOS IS-04/05 (each tile a routable… | Multiview ingests and emits ST 2110-20/-30/-40 locked to PTP, with ST 2022-7 hitless across two NICs proven under path-loss injection; registers/discovers via NMOS IS-04/05 and is patchable by an external controller; control APIs securable… | 📋 Planned |

> **M0 is delivered by this repository:** the design docs, ADRs, agent-instruction system, compiling
> workspace scaffold, CI, dev container, and example configs. Everything M1+ is implementation against
> the documented contracts. See [`CONTRIBUTING.md`](CONTRIBUTING.md) and the
> [agent guardrails](docs/development/agent-guardrails.md) before starting a milestone.

> **Current state (foundation build-out, in progress).** Beyond M0, the pure-Rust foundation is
> built and tested: all 16 crates compile, the default GPU-free / native-dep-free build passes the
> full CI gate set (fmt, clippy `-D warnings`, test, `cargo deny`, inclusive-language), and there are
> ~500 tests (unit + property + integration). Concretely landed:
> - **`multiview-core`** types/traits expanded; the **10 leaf crates** built with real tests; the
>   **`multiview-engine`** protected output core (output clock, compositor drive, `EngineRuntime`,
>   arc-swap + drop-oldest broadcast isolation, supervisor, degradation loop) with **invariants #1
>   and #10 exercised by tests**.
> - **Layer C:** `multiview-control` (axum REST/WS/SSE, OpenAPI, SQLite, command bus, auth),
>   `multiview-preview` (isolated taps), `multiview-cli` (`validate` + `run --headless`); `cargo xtask
>   gen-openapi` emits the spec. The **web SPA** has its design system, react-konva/dnd-kit layout
>   editor, realtime client, and i18n/a11y scaffolding.
> - **Feature-gated, NOT yet CI-gated and NOT verified on hardware here:** the **wgpu** GPU
>   compositor (`wgpu` feature) and the **FFmpeg** media path (`ffmpeg` feature). NDI remains
>   design-only.
>
> So the in-progress milestones above reflect **partial** delivery, not completion. The headless
> software engine runs and produces frames on the output clock, but the end-to-end software
> decode→compose→serve pipeline (M1), real hardware decode/encode and on-device GPU compositing (M2),
> fault-injection/soak SLOs (M3), the full I/O protocol set (M4), and the published density numbers
> (M8) are **not** yet done. None of M1–M8 is complete; no milestone flips to ✅ until its exit
> criteria are genuinely met. **Broadcast milestones M10–M12 have not been started.**

Milestones are sequential but overlap where dependencies allow (e.g. M6/M7 control-plane work can
begin against the M3 engine). Re-check the two load-bearing invariants — **#1 output-clock** and
**#10 isolation** ([conventions §5](docs/architecture/conventions.md#5-canonical-technical-invariants)) —
at every milestone; a change that risks either is a stop-and-design-note event.

## Broadcast multiviewer extension (M10–M12)

Add three cohesive new milestones after the current M9 for the professional-multiviewer delta, and fold smaller enhancements into existing milestones M3/M4/M5/M6/M9. Sequencing: M10 (monitoring/alarm engine) first — highest value, builds on the existing per-tile state machine and alert_card; then M11 (UMD/TSL/tally/salvos/operator UX), which depends on M10 alarms and the M7 UI; then M12 (heaviest: native IP/ST 2110/PTP, NMOS/router control, multi-head walls). The compositor, layout, overlay-render, audio (R128), caption, colour, resilience-state and API/preview foundations are already designed, so these milestones add only the broadcast monitoring + control plane on top. All capabilities are vendor-neutral and anchored in open standards.

These milestones add **established, standards-based** broadcast-multiviewer capabilities. See [FEATURES.md](FEATURES.md), [docs/research/broadcast-multiviewer-features.md](docs/research/broadcast-multiviewer-features.md), and decisions [ADR-MV*](docs/decisions/README.md#broadcast-multiviewer).

**Capabilities folded into existing milestones:**

- **M3:** Per-tile source crop/zoom (region-of-interest) in the layout model + compositor; Broaden layout presets (4x4, 2+8, n+m mixed) and add picture-outside-picture (PoP); Validate arbitrary/overlapping free-form tile geometry and raise the per-head PiP-count ceiling
- **M4:** Selectable audio meter ballistics/scales (PPM Type I/IIa/IIb per IEC 60268-10, VU, sample-peak per IEC TR 60268-18, true-peak dBTP per ITU-R BS.1770); Expose R128 sub-meters (M/S/I/LRA/max-TP) + selectable ATSC A/85 profile + per-channel and program-bus loudness; Phase/correlation + goniometer + surround grouping with Lo/Ro-Lt/Rt downmix metering (ITU-R BS.775); Multi-channel (16+) metering + channel mapping/shuffle/de-embed matrix; audio-follow-video monitor/PFL bus; MPEG-TS full PSI/SI parsing (PAT/PMT/NIT/SDT/CAT/TDT/TOT) + MPTS program selection; Confirm SRT Caller/Listener/Rendezvous + AES encryption + stream-id; add MPEG-DASH ingest + ABR-ladder awareness; add WebRTC ingest; explicit NDI HB+HX+HDR handling

### Backlog — Live YouTube input source (yt-dlp resolver)

📋 **Backlog (not started), folded into M4 (Full I/O).** Add a `youtube` source kind that resolves a YouTube watch/live/channel URL into a concrete HLS master-playlist URL via an **external, runtime-discovered `yt-dlp` binary** (off-by-default `youtube` feature; binary operator-installed, **not vendored** — mirroring the NDI runtime-load + licensing posture), then feeds it into the existing HLS ingest path + input pacer + supervised reconnect, with **periodic re-resolution** before the ~6 h googlevideo URL expiry. Builds on the M4 HLS ingest work; depends on nothing new beyond it. Resolution failures degrade the tile (STALE → NO_SIGNAL), never the output clock. Design: [docs/io/youtube-live.md](docs/io/youtube-live.md). Decision: [ADR-0015](docs/decisions/ADR-0015.md) *(Proposed)*. This is **planned/backlog only** — not in progress, not done.
- **M5:** Safe-area / title-safe / action-safe / center-cross marker overlay (SMPTE ST 2046-1: 93%/90% of the Production Aperture; cite ST 2046-1 not RP 2046-2 for the percentages); Analog-face clocks + multiple styles + multi-timezone + NTP/PTP source selection with lock/ref-loss status; Extract embedded source timecode (ATC/RP-188/VITC/LTC) for per-tile display vs generated TC; Per-input HDR-format detect/override (PQ/HLG/S-Log3) + correct mixed HDR/SDR compositing (BT.2446)
- **M6:** Finer RBAC scoping (admin/read-only/output-scoped roles) + change audit log + config versioning; OAuth2/JWT auth option (aligned to NMOS IS-10 where NMOS is adopted)
- **M9:** Formalize HA model: active/standby + N+1 engine instances with heartbeat health-check + automatic output failover + state replication
