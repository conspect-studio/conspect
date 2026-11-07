# Source events implementation ledger (#183)

Status: a working, operator-testable slice is shipped on `feat/rc-events`. Base
8a2a9725 + the merged `feat/source-event-automation-183` foundation work
(HLS/SCTE-35/ID3 observation, core tracker, overlay projection, read API,
web Events panel). This ledger records what changed to make it visible and
usable, not the full original automation scope (see "Not done" below).

## What is shipped and operator-testable

1. **Discoverability.** The Events panel is reachable from the main nav (the
   Sources page's Events column) and from each live source's monitoring tile
   (Monitoring page). The panel's empty state explains what is observed
   (HLS cue/DATERANGE/SCTE-35/timed ID3) and shows the source's current
   observation status: `observing` / `disabled` / `not_supported`, computed
   client-side the same way the engine gates it.
2. **Explicit opt-in config, default ON for HLS.** `Source.events.observe`
   (optional, default `true`) gates whether the engine activates the event
   tracker for an HLS/`youtube` source; every other kind never observes
   regardless of the field. Default-on is deliberate: for HLS this is
   read-only metadata parsing on the existing fetch/demux path (no extra
   network request, no extra disk write) — not a default cost. Automation
   stays disarmed independently of this flag. Toggling `observe` on an
   already-live HLS source goes through the canonical live-apply path
   (`PUT /api/v1/sources/{id}`, `X-Multiview-Apply: live`), which restarts
   that source's ingest producer — it is intentionally NOT folded into the
   `guide_only_change` metadata-only set, because it changes what the
   tracker's per-connection adapter attaches to.
3. **Overlay wired in production.** `StreamBaker::draw_list_with_events` (the
   badge/countdown renderer) is called from the real bake path in
   `pipeline.rs`, fed by `source_event_runtime::RuntimeReader::sample` off the
   output clock (one `SwapCell` read + no allocation on the hot loop). A tile
   showing an active/announced event renders its badge and clears
   automatically on confirmed end or epoch invalidation.
4. **Test-injection aid.** `POST /api/v1/inputs/{id}/events/test`
   (administer role, per-object authorization, audited) submits a
   clearly-labelled synthetic observation (`origin: external`,
   `event_id: "test:break"`) through the SAME bounded worker queue and
   validation real evidence uses. It requires an already-active generation
   (the source must have been ingested at least once) — it cannot allocate a
   new source identity, so it is not a general evidence-submission surface.
   The web Events panel exposes "Inject test break start/end" buttons,
   labelled "Test aid (admin only) — not live evidence", and a `TEST` badge
   on any `test:`-prefixed row.

## Not done (explicitly out of scope for this slice)

Per-source automation rules (armed take/restore into bars or a target
source, with a manual "restore now" override) are **not** implemented in
this slice — the pure safety-model library (`event_automation.rs`,
`SafetyModel`) and the `Command::SwapSource`/routing primitives it would
drive both already exist, but no route/UI arms it. This was cut to ship
(1)-(4) fully rather than (4)+automation partially, per the ship-rescue
brief's explicit fallback. A follow-up slice should: add a per-source rule
resource (disarmed by default) backed by `event_automation::SafetyModel`,
poll `expired_leases`/matching-end evidence on a control-plane task (never
the clock thread), execute via `Command::SwapSource` (cell-level swap,
the documented minimum) or a `Command::Route*`, and add the arm/disarm
toggle + "restore now" button to the web panel.

## Coordination

Compositor `crates/multiview-compositor/src/overlay/subpass.rs` internals
were NOT touched (another worker owns performance work there) — only the
existing public `draw_list_with_events`/`push_tile_watermark`-style APIs
were called. EPG/playlist and weather code were not touched.
