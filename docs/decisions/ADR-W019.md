# ADR-W019: Live apply of stored layouts — resolve at the route, swap at the frame boundary

- **Status:** Accepted
- **Area:** Web/API stack · layout management ↔ engine (invariant #11 Class-1)
- **Date:** 2026-06-10
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md);
  builds on ADR-R004 (atomic scene-graph swap at a frame boundary), ADR-W008 (202 + operation id),
  ADR-0034/RT-6 (O(1) crosspoint re-points), and invariants #1/#10/#11

## Context

`POST /api/v1/commands/apply-layout {layout: id}` always returned `202`, but the engine's
frame-boundary command drain could only re-solve the **config** layout: any stored layout id from
the control plane's layouts repository (`{canvas, layout, cells}` bodies the WebUI saves) was
logged as *"unknown layout id (no named-layout library yet); ignored"*. "Save & apply to engine"
silently no-op'd for every layout the operator actually edits. Management changes must apply
**instantly** to the running engine (invariant #11 Class-1), not via export+restart.

The drain runs **on the render thread** at the frame boundary: it must never do blocking I/O
(repository reads), never lock a contended lock, and never adopt a layout that could stall or
invalidate the output (invariants #1/#10).

## Decision

1. **Resolve and solve at the route, off the hot path.** `cmd_apply_layout` fetches the stored
   body from the layouts repository **at request time**, parses it as a typed
   `multiview_config::LayoutDocument` (`{canvas, layout, cells}` — tolerant of the editor's
   minimal canvas: `width`/`height`/`fps` required, the rest pinned at session start), solves it
   to a named `multiview_core::layout::Layout` (the stored id becomes `Layout::name`), and
   validates the solved geometry (`Layout::validate` + unique non-empty cell ids). Any failure —
   unknown id, unparseable body, unsolvable grid, invalid geometry — is `422
   application/problem+json` **before** any `202` is issued. Honest API: a `202` now means the
   engine will actually swap.
2. **The command carries the solved artifact.** `Command::ApplyLayout` gains
   `document: Option<Box<ResolvedLayout>>` — the solved core layout plus the typed document —
   populated by the route. The frame-boundary work is **O(cells) and allocation-light**: one
   `CompositorDrive::set_layout` (pointer swap after a pure re-validate), one `set_cell_ids`, one
   `set_cell_slates`, and a working-config mirror. No repository read, no `.await`, no contended
   lock on the render thread. **Back-compat:** a command without a document falls back to the
   prior behaviour (re-solve the working config iff the id matches its solved name).
3. **Class-1 gate: the canvas is pinned (ADR-R004).** A stored layout whose
   width/height/cadence differ from the running session's canvas is a Class-2 change (output
   geometry/cadence is pinned for the session's life) and is refused with `422` at the route —
   compared against the **immutable pinned-canvas snapshot** (`AppState::running_canvas`)
   captured from the loaded config at seed time, **never** the mutable layouts repository (a
   `PUT` on the working layout cannot move the gate). When no snapshot was seeded the gate
   **fails closed** (`422` "running canvas is unknown") for document-carrying applies — a `202`
   must never decay into a silent drain hold. Cadence equality is **by value**
   (`Fps`/`Rational::PartialEq` cross-multiply in `i128`; the drain compares with
   `Canvas::same_signal`), so a non-reduced `50/2` matches a running `25/1`. The drain keeps the
   authoritative backstop against the live `drive.layout().canvas` — the output clock never
   adopts a canvas the encoder session was not built for.
4. **Snapshot semantics.** The command ships the **request-time** resolve: a layout edited (or
   deleted) between the route fetch and the frame-boundary apply does not affect the in-flight
   command — there is no `If-Match` on apply; the next apply ships the new body.
5. **Idempotency reserves before resolution.** A replayed `Idempotency-Key` answers from the
   reservation — the original operation id, `kind: "replay"`, **no**
   `applied_live`/`carried_only` decoration — without re-resolving the layout (even if it has
   since been deleted: the original command already reached the engine, and the retry asks "did
   it land?"). A fresh request whose resolution is refused (`422`) **releases** the reservation,
   exactly like the shed-on-full path, so a corrected retry with the same key actually submits.
6. **Unknown/unbound sources never falter the output.** `set_layout` swaps bindings wholesale;
   a cell bound to a source with no registered `TileStore` (no running ingest) composes its
   per-cell `on_loss` failover slate via the existing `sample_cell` path — never a panic, never a
   stall, consistent with `SwapSource`'s store-gated semantics (which *holds* a re-point; a
   wholesale swap *binds and slates* — the cell is honest about the missing feed).
7. **Observability.** On a successful swap the drain emits `Event::JobProgress{phase:
   "apply_layout", pct: 100, message: <id>}` on the `jobs` topic (drop-oldest, inv #10) plus a
   `tracing::info!`; the proof remains the next composited frame. A drain-side **hold** (the
   pinned-canvas backstop, or a compositor rejection) is equally observable: a
   `JobProgress{phase: "apply_layout_held", pct: 0}` outcome naming the layout and the reason —
   a broken promise is never only a log line. Events ride uncorrelated (`corr: None`), like the
   other frame-boundary route commands. The `202` body states `applied_live` vs `carried_only`
   property classes.

### Which per-cell properties apply live

Only what the solved layout / compositor drive already carries is claimed (no invented features):

- **Applied live:** geometry (grid `area` / absolute `rect` → normalized rects), z-order, source
  bindings (`source.input_id`), per-cell `opacity`, per-cell `on_loss` failover slate
  (`set_cell_slates` → composited for down tiles).
- **Carried but not yet rendered** (stored, exported, *not* composited by the current drive):
  `fit`/`align` (the drive scales-at-composite into the cell rect — fill semantics), `border`,
  `qos`, `corner_radius`, `scaler`, `visible`, `static_friendly`, `label`, `rotation`, and the
  canvas `pixel_format`/`background`/`color` (pinned at session start). These remain in the
  document and the working-config mirror; rendering them is compositor work, not plumbing.

## Alternatives considered

- **Drain reads the repository at the frame boundary** — rejected: blocking I/O / lock on the
  render thread (inv #1/#10).
- **Ship only the raw JSON body and solve in the drain** — workable (solving is pure) but moves
  parse+solve+validate failures past the `202`, recreating the silent no-op; solving at the route
  keeps the drain to the swap.
- **Apply a mismatched canvas by re-basing rects onto the running canvas** — rejected: silently
  rendering a layout authored for another canvas mis-states operator intent; the honest answer is
  the Class-2 refusal (parallel-output migration is ADR-R004's follow-up lane).

## Consequences

- Stored layouts (grid or absolute) apply live at the next frame boundary; the WebUI's
  "Save & apply" is real. `202` is a promise, `422` is honest.
- The command bus now carries a per-command payload of O(cells) size (bounded by the bounded bus,
  cap 64ish commands — bounded memory holds).
- The drain's working-config mirror keeps `ApplyLayout`-fallback/export/salvo surfaces coherent
  with the active layout.
- Per-cell `on_loss` is now honoured by drain-wired runs from the first tick (the config's
  declared slates are pushed to the drive one-shot), making the schema's documented behaviour real
  on the control-enabled path; the headless pipeline path is unchanged (pre-existing gap, tracked
  in the work schedule).
