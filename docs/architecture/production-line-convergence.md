# Production-line convergence onto main (R3 design note)

Status: implemented in the PR that lands `feat/approved-broadcast-desk` on `main`
(supersedes PR 224). Change class: **R3** (touches the data plane in
`crates/multiview-cli/src/pipeline.rs`).

## What converged

| Side | Commit | Content |
|---|---|---|
| `main` | e40f08c4 | FFmpeg 9.0.2 / ffmpeg-next 9 (PR 218), shared output runtime + MoQ output/preview (PR 217, curated as 3f8e2c0e from the same lineage as the desk copy, then reviewed and CI-gated) |
| desk | b38bb5ac | main@7f7d38ed + the production line (IPTV, graphics, weather, source events + journal/automation, demand-driven source lifecycle, output link/playback, display-follow decode, SRT listener input) + Program/Preview switcher first slice |
| PR 224 | c062f3f7, 654648c9, d19d51d8, f34ba152, 7c034ea7 | switcher fixes: committed video/audio alignment, pre-CUT audio across shed ticks, lint |
| 344dcd51 | | demuxer parameter snapshots detached from input lifetime (code already on main; test + note added) |

Rule applied everywhere: nothing is dropped; where both sides carry the same
work, main's copy is authoritative and only the desk's functional additions
are re-applied on top.

## `pipeline.rs` conflict regions (diff3 numbering of the merge)

| # | Region | Winner | Why |
|---|---|---|---|
| 1 | imports | union | main's `WindowDelivery` trait + desk's `display_size` imports are both used. |
| 2-3 | `StreamPlan` in `run_for_publishing` / live run | main | main's `build_sink_runners` returns `(runners, binding_ids)`; the desk's extra fields (`consumer_fault_after_ticks`, `shed_at_tick`) are present in both. |
| 4-5 | `build_sink_runners` | main | Same lineage; main adds `binding_ids` (final runner order, `program.ts` anchor internal) that the MoQ/WHEP registry needs. Desk's push-link publication is identical in both. |
| 6 | `StreamEgress::spawn` call | main | Adds registry axes, subscription slot, optional preview; desk's gates/media span/cadence are identical in both. |
| 7 | snapshot inventories | desk | Live inventory map (on-demand sources publish their own inventory when started) is desk-only functionality; main's boxed push-link state is identical. |
| 8-9 | `spawn` signature / lint allows | main | Superset signature; desk's allows are covered by main's combined `allow`. |
| 10 | egress setup | main | Desk's text is a strict subset; main adds the `OutputWindowTap` mirroring every admitted window to the effective-output registry **before** physical admission so a stalled SRT/HLS sink can never starve a MoQ/WHEP subscriber (inv. #10). |
| 11-12 | sink loop / consumer args | main | identical / superset. |
| 13 | `egress_fatal_tests` | main | Identical except one assertion form (`sink_frames == Vec::new()` vs `.len() == 0`); same strength. |
| 14 | `consumer_main` params | main | Generic `D: WindowDelivery` (tested with fakes) instead of concrete `BindingDelivery`. |
| 15 | worker/dispatcher setup | main | Superset (optional preview ingress). |
| 16 | per-tick dispatch | main | Superset: adds `delivery.pump(now)` (off-clock, never waits). |
| 17 | encode offer + substitution | main | Identical no-falter logic; main additionally offers the canvas to the optional preview (non-blocking `offer`). |
| 18-20 | audio dispatch + finish | main | Same events; main also mirrors them to the optional preview. |
| 21 | `SinkSendPolicy` | identical | |
| 22 | SRT listener URL unit test | desk | Desk's test is correct for both feature states (`srt-libsrt` on: lowered URL; off: fails closed with `ListenerRequiresNativeLeaf`). Main's unconditional form would fail on the default build now that the desk's fail-closed listener lowering is in. |
| 23 | trailing test modules | union | main's `window_subscription_pipeline_tests` + desk's `http_headers_tests` and `display_follow_tests`. |

Auto-merged desk code that enters the tick path's neighbourhood (all already
reviewed on the production line and running on mda001): demand-driven source
rails, display-follow decoder handover, switcher crosspoint in the drain,
switcher audio selection in the bake consumer.

## PR 224 re-application

- c062f3f7: kept program-audio selection following the committed video, PGM/PVW
  audio warm set, switcher control route additions, and all tests. Its
  `invalidate_live_edit` fail-closed edit guard was written for a tree without
  demand-catalogue live edits; the converged tree restarts an edited source
  through the `DemandManager` (catalogue upsert + `restart`), which already
  prevents a stale rail. The removal-time pixel retirement it introduced is kept
  (`clear_on_remove`, switcher runs only). See PR Concerns.
- 654648c9: `drive_selected_audio_for_item` catches up shed ticks under the
  prior selection before applying the survivor's selection; stale items never
  change gains.
- d19d51d8 / f34ba152: lint-only; 7c034ea7 was already in the desk tree.

## Invariants

- **#1 output clock.** The hot loop is unchanged from main: one item per tick
  onto a bounded hot queue (`try_send`, shed on full). The consumer keeps main's
  no-falter frame substitution and the `FRAME_SUBSTITUTION_LIMIT` envelope.
  PTS/DTS come from the tick index (`encode_frame_at(frame, tick)`), never float fps.
- **#10 isolation.** Every subscriber path added by either side is bounded and
  non-blocking: `OutputWindowTap` admission, `delivery.pump`, preview `offer`,
  switcher command mailbox, audio selection (wait-free `SwapCell`).
- Evidence: see the PR's verification section (cadence/isolation/chaos/shed
  suites, switcher first-slice and audio suites, window-subscription pipeline
  tests). Long soak runs on mda001 after deploy.
