# Live source add/edit on a switcher run (issue #464, R3)

Status: accepted (lane livesrc, 14 Nov 2026). Invariants #1 and #10.

## Problem

On a run that declares a first-slice M/E (`[switcher]`), every live source
upsert of a decoded kind (and every synthetic kind, since a demand-managed run
catalogues all kinds) reached the live-source hub, which asked
`IngestSpawner::invalidate_live_edit`. `LiveIngestSpawner` answered `true` for
**every** id when `clear_on_remove` (switcher run) and a demand manager were
present. That removed the catalogue entry and the demand pins and logged
`switcher source edit requires restart`. So:

- a **new** source id was never catalogued: `POST /sources/{id}/start` answered
  `409 ... applies on restart`, while the create had answered
  `X-Multiview-Apply: live` (mda001, 15 Nov 08:31 AEDT, `youtube-germnay`);
- an edit of any source silently dropped it from the running engine.

## Why the fence existed

`git log -S invalidate_live_edit`: c062f3f7 (PR 224) added it; f60d96d2 dropped
it in the production-line convergence; 3dcc363d restored it after review
(openai/gpt-6-sol on de03eab4, HIGH): *the demand-catalogue live-edit path
reuses the same `TileStore` and only schedules an asynchronous restart, so an
opted-in M/E could keep resolving the old source's pixels until the replacement
publishes (or indefinitely on restart failure).* The finding is about **edits**
(stale media of the previous definition). A new id has no previous definition
and no stale pixels; refusing it was over-broad.

## Rule

1. **Add (id not catalogued).** The hub catalogues it exactly as on a
   non-switcher demand run: catalogue entry with the drain's store, standing
   pins, restart request. The demand worker starts it off the clock. Its store
   is also published into a live bus-store map (below), so the M/E can prime
   and take it. Nothing on air is touched.
2. **Edit of a media-changing field.**
   - **On a bus**: the control plane refuses it **before the durable commit**
     with `409` naming every switcher consumer (Program / Preview of the M/E,
     fill / key of a downstream keyer). Nothing is stored, nothing is
     enqueued. The operator takes the source off the bus first. Make-before-
     break into the same store was rejected: the M/E would air the old
     definition until the new one publishes, the exact PR 224 finding.
   - **Not on a bus**: applied live. After the hub's teardown has stopped and
     awaited the old producer (bounded), the spawner advances the source's
     lifecycle (fencing any detached writer), clears the store (no stale
     pixels can be primed or taken), then re-catalogues the new definition into
     the same store and requests a restart. The tile shows the slate until the
     new definition publishes, never the old media.
   - Policy/guide-only edits keep the existing no-restart path.
3. **Race.** A take onto the source between the control-plane check and the
   hub's retire is possible. The store is then cleared under it: Program shows
   the honest no-signal slate of a `starting` source until the new media
   arrives. It never shows stale media and is never silent (source status).
4. **Start agrees with the engine.** A source the engine adopted live (its
   upsert landed) but the hub has not catalogued yet is waited for, bounded
   (2 s, off the engine, on the control task), then started. Only a source the
   engine really does not run gets `409 ... applies on restart`.
5. **Remove** is unchanged: retire pixels, drop pins, fence.

## Live bus-store map

The switcher tick driver's prime map and the PVW monitor map were fixed at run
start, so a live-added source could never be primed or monitored. The pipeline
now owns a `SwapCell<Arc<HashMap<id, store>>>` the spawner RCUs on add/remove
(hub worker only). The driver and the hook consult it first, then the fixed
boot map. On the clock this is one wait-free `SwapCell::load` and a hash lookup
per freshness check: no lock, no allocation, no await.

## Invariants

- **#1.** No new work on the output clock beyond the wait-free load above. All
  spawn, teardown, clear and catalogue work stays on the hub worker and the
  demand worker.
- **#10.** No new queue. The hub queue (32) and the command bus stay bounded and
  shed; the Start wait is a bounded poll on the control task, never the engine.
- Evidence: `live_source_switcher_` lib tests (add catalogues, edit retires,
  on-bus refusal) and the chaos test `switcher_live_source_chaos` (20 adds
  under a command and M/E flood on a deterministic clock: contiguous ticks,
  tick-derived PTS, zero repeats, every added source ready).
