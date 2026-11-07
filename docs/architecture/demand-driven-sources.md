# Design note — source activation and demand-driven sources

Status: Implemented (branch `feat/rc-demand-driven-sources`) · Class: R3 ·
Invariants: #1 (output clock), #10 (isolation), per-object authorization

## Requirement

Operator decisions of 2026-11-07 (ship-rescue `srt-addendum.md`):

- **Default is READY.** `sources[].activation = "always_on"` (the default)
  starts a source at boot whether or not anything consumes it, exactly as
  production does today. `"on_demand"` is the per-source opt-in: idle (zero
  cost) until something uses it, stopped again `close after` the last user.
- "500 sources cost nothing" holds for sources set `on_demand`. Bulk M3U
  import makes the operator choose; there is no silent default.
- States are `idle` / `starting` / `ready` / `failed`. This is a separate axis
  from the tile signal state (LIVE / STALE / NO_SIGNAL) and from tally.
- Actions are Start / Stop. Timers are "start timeout" and "close after".
- The IPTV guide keeps refreshing in the background while publication is
  enabled, every `[iptv] guide_refresh_minutes` (default 30, 5..=1440).

## Model

A configured source is a **catalogue entry**: its definition plus an empty
`TileStore`. An `always_on` source also holds a standing lease (consumer class
`always_on`), so it is built at boot and never closes. An `on_demand` source's
catalogue entry is its whole idle cost: no plan, probe, thread, socket,
decoder or timer.

Config:

| Key | Default | Notes |
|---|---|---|
| `sources[].activation` | `always_on` | `on_demand` opts in. Edited through the source API (`commit_resource` under `lock_config_mutation`), survives restart (`desired_source_activation.rs`). |
| `sources[].demand.always_on` | — | Read-only alias from earlier builds of this branch: `false` = `on_demand`. Never written back. `journal_events` is accepted and ignored. |
| `[system.demand] close_after_secs` | 30 | `linger_secs` is read as an alias. |
| `[system.demand] start_timeout_secs` | 20 | |
| `[system.demand] max_concurrent_starts` | 8 | |
| `[system.demand] max_stuck_opens` | 8 | Timed-out opens that have not returned (a hung native open cannot be cancelled) still hold a thread, deleted sources included. Opening plus stuck starts never exceed this; at the cap a new start reads `failed` ("too many stuck source opens") until one returns. A start that returns after its deadline is stale and stopped, whenever the worker next runs. |
| `[iptv] guide_refresh_minutes` | 30 | Validated 5..=1440; settable on the IPTV page. `guide_idle_secs` is removed. |

### Demand leases (engine: `multiview-engine/src/demand.rs`)

`DemandManager` sits on top of `SourceRegistry`. A consumer calls `acquire(id,
rails, tag, size)` and holds the returned `DemandLease`. Dropping the lease
releases the demand. Rails are modelled separately:

- `Rails::VIDEO`: the picture decode or generator, plus what rides on it
  (captions, event observation, the WHIP receive).
- `Rails::AUDIO`: the audio decode. **An audio-only lease never starts video.**
  It runs the audio on its own clock (`AudioStore::live`) because nothing stamps
  a video timeline to lock to.

A video lease also holds a registry `SourceHandle`, so the registry's refcount
and supremum size follow the consumers (decode-once, use-many). A **group** of
consumers (the active layout's cells, the audio routes, the subtitle routes) is a
`ConsumerSet`. Publishing a new set takes every new lease **before** dropping any
old one, so a source shared by the old and new layout never restarts.
`ConsumerSet::publish` is a wait-free cell store plus an unpark, so the
output-clock thread can call it.

Lifecycle, all off the output clock:

- **Start**: the first lease wakes the single `multiview-demand` worker. It spawns
  a transition on a transient thread that runs the run's `RailDriver`, which is
  the same `ingest_plan_for` / `spawn_ingest_producer` construction as startup
  and live-add.
- **Bounded concurrency**: at most `max_concurrent_starts` sources (default 8)
  may be starting at once. A start holds its slot until its first frame, or
  until `start_timeout` (20 s), when it reads as failed and frees the slot.
  Stops share a separate thread bound (2 × starts), so 500 last-releases never
  mean 500 threads. A deferred move runs when a slot frees.
- **Close after**: when the last lease goes, the rail keeps running for `close
  after` (default 30 s). A consumer that returns in time finds the rail still
  running, so a layout flip does not thrash. An operator Stop skips it.
- **Stop**: raise the rail's stop flags in the shared stop registry, then
  bounded-join (the hub's existing grace). A final stop (not a restart) clears
  the store so an idle source retains no frames.
- **Retry**: a failed transition backs off exponentially from 1 s up to 30 s,
  up to the kind's retry cap if it has one (none for today's kinds). At the cap
  the source stays `failed` ("gave up ... press Start") until a new consumer,
  an operator Start or an edit. A lease that cannot be satisfied (an audio
  route on an audio-free kind) never starts and reads idle, not failed.
- **Boot-started rails** are adopted, not restarted, and never hold a start
  slot, so eleven always-on sources awaiting first media never block an
  on-demand start.
- **Idle cost of the manager**: a source with no lease has no record. The worker
  parks with **no timeout** while nothing is active or pending. A single health
  timer (1 s) runs only while some source is active. There is no per-source
  timer.

Status per source is `idle`, `starting` (with `starting_phase` `queued` /
`opening` / `awaiting_media` and `deadline_in_ms`), `ready` or `failed`, plus
who uses it and its start `generation`. Ready means the store's publish
sequence, or the audio store's publish count, is advancing. Failed means no
progress within the start timeout, or within `stall_after` (15 s) once ready,
unless the source is finite (a file, or a static generator such as `bars`).

### Start / Stop (`POST /api/v1/sources/{id}/start|stop`)

Write role, `authorize_object` on the id, audited as a `command` with
`{"command": "start"|"stop"}`. Start takes a `manual` lease that holds the
source ready until Stop. Stop is **refused with 409** while any other
consumer holds the source, and the detail names every one (an `always_on`
setting is one: set the source `on_demand` to let it stop). The Sources page
shows Start / Stop in the status column and the refusal inline.

### Transition events (`input.lifecycle`, topic `inputs`)

Every phase change publishes `{source_id, generation, from, to, reason,
at_ms, lease_holders}` on the existing engine event path, object-scoped by
`source_id`. Emitted by the manager after its lock is released, into the
bounded drop-oldest publisher ring: a slow subscriber cannot stall it.

### Tile status chip

The chip keeps the signal state for ready sources (LIVE green, STALE /
RECONNECT amber, NO SIGNAL red). A lifecycle-managed source that is not ready
shows its lifecycle instead: `IDLE` (neutral grey, never red), `STARTING:
QUEUED|OPENING|WAITING` (neutral blue), `FAILED` (red). The bake thread reads
one `SwapCell` per frame; the map is replaced only on a transition, so steady
state allocates nothing. Media-player tiles (not catalogued) keep the signal
state. Tally borders/blocks and not-ready fills are not built here.

### Owner hooks for later kinds (`multiview-engine/src/lifecycle.rs`)

For web sources (not built): an async `SourceOwner` trait with `start(source,
generation, deadline)`, `stop(source, generation)`, an optional `kill`, a
wait-free `progress` and per-kind `KindBounds` (start timeout, close after,
retry cap). `OwnerRails` adapts an owner to the `RailDriver` the manager
drives, running owner futures on a tokio handle from the transient transition
threads (never a data-plane thread). A start past its deadline is cancelled
and fails. A successful start registers `{source_id, generation, kind}` in the
`ScopeRegistry`; it is removed only after the owner confirms stop. A stop past
its bound goes to `kill`; a failed kill keeps the scope and reads `failed`.
`ScopeRegistry::is_current` fences late samples from an older generation.
Covered by fake-owner tests.

### Consumers (verified against the code)

| Consumer | Where it takes demand | Rails |
|---|---|---|
| Layout cells (active layout) | Boot: `ConsumerSet` seeded from the solved layout. Runtime: the command drain republishes when `CompositorDrive::layout_generation` changes (`RouteVideo`/`SwapSource`, salvo takes, stored `ApplyLayout`). Sized per cell. | video + audio (the tile meter reads its audio) |
| Program-bus and discrete audio routes | Boot: `[audio].routes`. Runtime: `AudioControlHandle::request_routing` republishes on every live routing apply. Muted routes count, because the route is how an operator meters it. | audio |
| Subtitle routes (`RouteSubtitle`) | Command drain, per layer routed away from its own source | video (captions ride the video demux) |
| Source preview, WHEP | `CliWhepProvider::negotiate` (input scope) holds a lease for the session's life. A session whose peer dies is reaped by the encode pump (`whep_egress` now marks dead sessions closed), so a tab closed without `DELETE` still releases. | video |
| Source preview, JPEG polling (`/preview/inputs/{id}.jpg`) | Each poll takes or extends a 5 s timed lease (at most 64 timed leases). Until the first frame it answers 503 with `X-Multiview-Source-State: starting`. Monitoring's ~1 fps **thumbnails do not poll idle sources**, so opening Monitoring never starts all sources; opening a preview dialog does. | video |
| On-demand HLS sessions (`feat/rc-on-demand-hls`) | `SourceDemandProvider::session_lease(id, session)`. That branch must hold this lease for each session's life (see "Not done"). | video + audio |
| Armed event automation | Standing pins from `events.automation.armed`: the watched source (video, for observation) and the rule's replacement target (video, or video + audio for `route_program`) so a take has a picture at once | video / all |
| `activation = "always_on"` (default) | Standing pin from the source setting | video + audio (as the kind has them) |
| Operator Start | `manual` lease until Stop | video + audio |
| Live `UpsertSource` / `RemoveSource` | Not a consumer: an add or edit updates the catalogue and restarts the source only if something consumes it | — |
| Probes / fault detection, tile-state events | Not consumers. They sample only the cells the output composites, so they follow layout demand. An idle source never appears in them (proved by `idle_sources_raise_no_tile_state_or_alarm_events`). | — |
| Media-player channels | Not demand-managed: players are their own boot-spawned `IngestPlan`s from `media_players` (unchanged). No `SourceKind::MediaPlayer` link is consumed in the CLI run path today. | — |
| Graphics backgrounds | No graphics item references a source id in the schema. Graphics and weather sources are synthetic generators and follow layout demand like any source. | — |
| AES67 sources | Unchanged: a local multicast join on the program bus with no catalogue entry, started at boot as before | — |
| `GET /inputs/{id}/streams` | Not a consumer. An unconsumed source answers `409 not-probed` ("not probed yet: preview it to start it") instead of an empty or stale inventory. A file source started on demand is probed when its rail starts, and the snapshot re-folds. | — |

**Event observation** rides the video rail, so it runs whenever a source is
ready. An `always_on` source (the default) is always ready, which keeps its
event history; there is no separate `journal_events` opt-in. An `on_demand`
source is observed only while something uses it, or while automation is armed
for it (a standing lease).

### IPTV guide (`multiview-control/src/iptv`)

Unchanged from rc3 except the interval: the background refresher fetches due
feeds while publication is enabled, every `[iptv] guide_refresh_minutes`
(default 30) plus up to a sixth of it (at most 5 min) of jitter. The earlier
on-request fetch of this branch is reverted.

## Invariants

- **#1 (output clock).** The clock thread never starts, stops or waits for a
  source. A newly bound cell composites `NO_SIGNAL` (or its `on_loss` slate) from
  the empty store until its first frame; the store already gives that
  behaviour. The drain's per-tick cost is one integer compare. On a binding
  change it builds a vector of at most the cell count and does a wait-free
  publish. Startup prime-wait still covers only boot-bound sources.
- **#10 (isolation).** Every engine→outside path here is a wait-free cell store
  plus an unpark. The book mutex is held for bounded bookkeeping and never
  across `RailDriver::transition` (blocking joins and opens) or `store()`. Under
  it run only `resolve`, `progress` and `finite`, which are wait-free reads. A
  wedged source ties up one transition thread and its start slot until the start
  timeout. It never touches the clock, a sibling source or the worker.
- **Authorization.** `GET /sources/status` filters rows by
  `authorize_object` (the `list_sources` idiom), and `/sources/{id}/status`
  authorizes the id. Previews keep their existing per-scope authorization; a
  denied preview never takes a lease.

## Failure behaviour

| Failure | Behaviour |
|---|---|
| Source cannot open (dead origin, refused) | `failed` with an operator-safe reason; retries with backoff (1 s → 30 s); the cell shows `NO_SIGNAL`; the clock is unaffected |
| Source opens but never produces | `failed` after `start_timeout`; its start slot frees for others; keeps retrying |
| Live source stalls | `failed` after `stall_after` (not for files or static generators); the existing ingest reconnect bracket keeps trying |
| Producer will not stop | detached after the hub's 3 s grace (warned); the stop slot frees |
| Thread spawn fails | that transition fails and retries; nothing else is affected |
| Worker cannot spawn | logged; leases stay booked; boot-started sources keep running |
| Program bus absent (no `--program-audio`) | audio demand is unsatisfiable and reads `idle`, not `failed` |
| Unknown source id leased | nothing starts; the record is dropped when the lease goes |

## Back-compat

Existing configs load unchanged and behave as before: every source starts at
boot (`demand_production_shape.rs`: 11 sources, 6 bound, `msnow` audio-routed,
all start). New fields are optional. Documents written by earlier builds of
this branch (`demand = { always_on = ... }`) still load.

## Interaction with other branches

- `perf/rc-nvdec-relayout` (merged): a demand start sizes the decode to the
  registry supremum of its leases, otherwise the display follower's size (the
  layout supremum, or 640x360 for an unbound consumed source, which stays). The
  relayout hook (`publish_display_layout`) also fires when the layout consumer
  set republishes.
- `feat/rc-on-demand-hls` should take `session_lease` per session; without it a
  session on an idle source sees `NO_SIGNAL`.
- `fix/rc-srt` changes output sinks only; no interaction.
- `feat/rc-event-journal`: see the observation tradeoff above.

## Tests

- Engine (`demand.rs`, counting fake driver): idle sources cost no record and no
  transition; first lease starts and last lease stops only `close after`; a
  return in time does not restart; audio-only starts no video;
  unsupported rails never spin; a set swap keeps shared sources; rapid
  acquire/release from 8 threads leaves nothing in the book or the registry; a
  failing source backs off and reports its reason; 500 demands never exceed the
  start bound; a never-producing source frees its slot; edits restart only
  running sources; pins replace new-before-old; a seeded set holds leases before
  the worker runs; adopted boot rails are not restarted.
- Engine: Start/Stop (refusal names consumers), transition events with
  generation and holders, retry cap, starting phase/deadline, zero idle
  wakeups, boot rails hold no start slot; fake-owner tests for `lifecycle.rs`.
- CLI (`tests/demand_production_shape.rs`): the production shape boots all 11;
  three `on_demand` start only on lease; 500 `on_demand` add no connection or
  thread.
- CLI (`tests/demand_idle_sources.rs`): 500 idle `on_demand` HLS sources (half with
  captions) make zero connections to a counting origin, start zero per-source
  threads and cost ~2.3 KB each; audio-only boot for a routed, unbound source;
  `always_on`; idle sources emit no tile-state or alarm events (the test fails
  when an idle source is bound).
- CLI (`tests/demand_lifecycle.rs`): a real run routes a cell to an idle source,
  which starts and goes ready; the abandoned source runs `close after`, then
  stops; routing back restarts it; a shared, always-on source stays ready. **Chaos:** four
  threads churning ~8,000 acquire/release cycles during a live 25 fps run emit as
  many frames as the same run without churn, and leave nothing wanted.
- Control: status lists every source (idle is not a fault); unknown is 404; an
  idle source's streams read `409 not-probed`; a JPEG poll takes a lease and
  answers `starting`. IPTV: 500 entries fetch nothing until requested; refresh
  stops after the idle period; a cold request is bounded; last-good is kept.
- Web: idle shows "Idle" with "costs nothing", never "No signal"; an idle
  thumbnail never polls; the badge names consumers and failure reasons.

## Measured (this dev container, debug build)

| | value |
|---|---|
| `Pipeline::build`, production shape, 11 always_on | 6-14 ms |
| same + 500 `on_demand` sources | 14-23 ms; +1.3-1.4 MB RSS (~2.5-2.8 KB per source); 0 connections; 0 extra threads |
| demand worker wakeups, 11 active sources | 1/s (the single health timer) |
| demand worker wakeups, everything idle | 0 (parks with no timeout) |
| allocations per lease acquire+release on a ready source | ~3.6-4.0 (LD_PRELOAD malloc counter, 10k-20k cycles; includes the worker pass each lease wakes) |

## Not done / needs host verification

- **Host:** production boot with this build: all 11 sources start as today;
  NVDEC sessions and RSS on the release build; set three sources `on_demand`
  and confirm they idle and start on preview.
- `feat/rc-on-demand-hls` must call `session_lease`.
- Tally borders/blocks, not-ready fill, last frame hold, VT, browser pools and
  web sources are out of scope.
