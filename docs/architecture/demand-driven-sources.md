# Design note — demand-driven sources

Status: Implemented (branch `feat/rc-demand-driven-sources`) · Class: R3 ·
Invariants: #1 (output clock), #10 (isolation), per-object authorization

## Requirement

> "by on demand - I don't mean - added to the IPTV playlist. I mean - only when
> there is a consumer. We shouldn't consume any resources - even if the user
> add's 500 sources to the IPTV playlist - until something tries to consume a
> source"

Before this change, `Pipeline::build` built ingest for every configured source.
On the production host, 11 sources were configured and 6 bound, yet all 8 network
sources opened a decoder. File sources were probed, HLS caption renditions were
fetched, and synthetic sources ran generator threads, all at boot. The source
registry's refcount existed but production used only its actor-less store path.

## Model

A configured source is a **catalogue entry**: its definition plus an empty
`TileStore` (a few atomics and an empty ring). That entry is its whole idle cost.
The source has no plan, probe, thread, socket, decoder or timer. Measured
(`tests/demand_idle_sources.rs`, unoptimised build, this host):

| | 0 idle sources | 500 idle sources |
|---|---|---|
| `Pipeline::build` | ~3 ms | ~7 ms |
| connections to the sources' origin | 0 | **0** |
| producer threads after a run | 1 (the bound `bars` generator) | **1** |
| process threads after a run | 17 | 8 (same order; scheduler noise) |
| RSS per 500-source pipeline, build only | — | **~1.1 MB (~2.3 KB per source)** |

The ~2.3 KB per source is the parsed `Source` document, its catalogue entry and
an empty store: O(config size). One decoded 1080p NV12 frame is ~3 MB.

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
- **Linger**: when the last lease goes, the rail keeps running for `linger`
  (default 30 s, `[system.demand] linger_secs`). A consumer that returns within
  the linger finds the rail still running, so a layout flip does not thrash.
- **Stop**: raise the rail's stop flags in the shared stop registry, then
  bounded-join (the hub's existing grace). A final stop (not a restart) clears
  the store so an idle source retains no frames.
- **Retry**: a failed transition backs off exponentially from 1 s up to 30 s. A
  lease that cannot be satisfied (an audio route on an audio-free kind) never
  starts and reads idle, not failed.
- **Idle cost of the manager**: a source with no lease has no record. The worker
  parks with **no timeout** while nothing is active or pending. A single health
  timer (1 s) runs only while some source is active. There is no per-source
  timer.

Status per source is `idle`, `starting`, `live` or `failed`, plus its consumers.
Live means the store's publish sequence, or the audio store's publish count, is
advancing. Failed means no progress within `start_timeout`, or within
`stall_after` (15 s) once live, unless the source is finite (a file, or a static
generator such as `bars`).

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
| Event journaling | Opt-in `demand.journal_events = true` standing pin | video |
| Explicit `always_on` | Opt-in `demand.always_on = true`: the escape hatch for "keep it hot" | video + audio |
| Live `UpsertSource` / `RemoveSource` | Not a consumer: an add or edit updates the catalogue and restarts the source only if something consumes it | — |
| Probes / fault detection, tile-state events | Not consumers. They sample only the cells the output composites, so they follow layout demand. An idle source never appears in them (proved by `idle_sources_raise_no_tile_state_or_alarm_events`). | — |
| Media-player channels | Not demand-managed: players are their own boot-spawned `IngestPlan`s from `media_players` (unchanged). No `SourceKind::MediaPlayer` link is consumed in the CLI run path today. | — |
| Graphics backgrounds | No graphics item references a source id in the schema. Graphics and weather sources are synthetic generators and follow layout demand like any source. | — |
| AES67 sources | Unchanged: a local multicast join on the program bus with no catalogue entry, started at boot as before | — |
| `GET /inputs/{id}/streams` | Not a consumer. An unconsumed source answers `409 not-probed` ("not probed yet: preview it to start it") instead of an empty or stale inventory. A file source started on demand is probed when its rail starts, and the snapshot re-folds. | — |

**Event observation tradeoff:** observing HLS cues for a source nothing consumes
would mean keeping its fetch running, the exact cost this change removes. The
default is therefore **no observation for an unconsumed source**. Observation runs
whenever the source runs for any other reason. It is also kept running on purpose
when automation is **armed** for that source, or when `demand.journal_events` is
enabled. `feat/rc-event-journal` records only what is observed, so a journal of
an idle source is empty until one of these holds.

### IPTV guide (`multiview-control/src/iptv`)

The refresher no longer fetches on its own. A publication with 500 entries
fetches **nothing** until `/iptv/guide.xml` (or `.gz`) is requested
(`five_hundred_entries_fetch_nothing_until_a_guide_is_requested`). A request
records itself and wakes the refresher. Only when nothing has been attempted for
the publication does it wait for the first pass, bounded by `GUIDE_COLD_WAIT`
(5 s). After that it serves what is cached: the last good guide, or channels with
no programmes. Refreshing continues on the existing 30 min schedule while guides
are requested, and stops after `iptv.guide_idle_secs` (default 6 h) without a
request. SSRF screening, size caps and timeouts are unchanged.

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

Existing configs load unchanged; every new field is optional
(`sources[].demand`, `[system.demand]`, `iptv.guide_idle_secs`). Sources bound in
the active layout, and sources with audio routes, start at boot as before. An
audio-routed source with unbound video (the production `msnow` shape) now runs
**audio only**. Sources that are neither bound nor routed (`dw-news`,
`weather-channel` and the unbound `clock` and `timer` on production) no longer
start until something consumes them.

## Interaction with other branches

- `perf/rc-nvdec-relayout` picks the decode size per source. Here a demand start
  sizes the decode to the registry supremum of its leases, then the boot layout,
  then the canvas. That branch's fallback for unbound sources is superseded:
  unconsumed sources do not decode. On merge, its relayout hook should run when
  the layout `ConsumerSet` republishes.
- `feat/rc-on-demand-hls` should take `session_lease` per session; without it a
  session on an idle source sees `NO_SIGNAL`.
- `fix/rc-srt` changes output sinks only; no interaction.
- `feat/rc-event-journal`: see the observation tradeoff above.

## Tests

- Engine (`demand.rs`, counting fake driver): idle sources cost no record and no
  transition; first lease starts and last lease stops only after the linger; a
  return inside the linger does not restart; audio-only starts no video;
  unsupported rails never spin; a set swap keeps shared sources; rapid
  acquire/release from 8 threads leaves nothing in the book or the registry; a
  failing source backs off and reports its reason; 500 demands never exceed the
  start bound; a never-producing source frees its slot; edits restart only
  running sources; pins replace new-before-old; a seeded set holds leases before
  the worker runs; adopted boot rails are not restarted.
- CLI (`tests/demand_idle_sources.rs`): 500 idle HLS sources (half with
  captions) make zero connections to a counting origin, start zero per-source
  threads and cost ~2.3 KB each; audio-only boot for a routed, unbound source;
  `always_on`; idle sources emit no tile-state or alarm events (the test fails
  when an idle source is bound).
- CLI (`tests/demand_lifecycle.rs`): a real run routes a cell to an idle source,
  which starts and goes live; the abandoned source lingers, then stops; routing
  back restarts it; a shared, always-bound source stays live. **Chaos:** four
  threads churning ~8,000 acquire/release cycles during a live 25 fps run emit as
  many frames as the same run without churn, and leave nothing wanted.
- Control: status lists every source (idle is not a fault); unknown is 404; an
  idle source's streams read `409 not-probed`; a JPEG poll takes a lease and
  answers `starting`. IPTV: 500 entries fetch nothing until requested; refresh
  stops after the idle period; a cold request is bounded; last-good is kept.
- Web: idle shows "Idle" with "costs nothing", never "No signal"; an idle
  thumbnail never polls; the badge names consumers and failure reasons.

## Not done / needs host verification

- **Host:** the production config end to end (11 sources: 6 bound start, `msnow`
  audio only, the rest idle); NVDEC under bounded starts; RSS on the real build.
- `feat/rc-on-demand-hls` must call `session_lease`; that branch was not edited.
- Standing pins are re-read on every live source upsert, and arming or
  disarming automation (`/inputs/{id}/events/automation/arm|disarm`) now submits
  that upsert. A run path with no live hub, or a shed submit, applies on restart.
- The software (`--no ffmpeg`) run path is unchanged: it has no decoders and
  renders every source from its pattern.
