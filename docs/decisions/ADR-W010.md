# ADR-W010: Canvas layout editor — accessible-equivalent non-canvas editing path

- **Status:** Proposed
- **Area:** Accessibility & Internationalization
- **Date:** 2026-06-02
- **Source:** [web/accessibility.md](../web/accessibility.md)

## Decision

Treat the react-konva layout editor as a presentational view of a single layout model (ordered cells: id, label, x, y, w, h, z, rotation) and build a fully-equivalent non-canvas editing path that drives the same model: an APG-Grid Cells list (roving tabindex, focus never lost on add/delete) plus a per-cell Inspector with numeric x/y/w/h/z/rotation inputs, +/- steppers, and explicit front/back/forward/backward buttons. Keyboard editing uses grab/move/drop/cancel (Enter/Space grab, arrows nudge, modifier+arrow resize, Esc restores) with a drawn pseudo focus ring on the canvas; dnd-kit (custom 1px/grid coordinateGetter, translated announcements) covers the DOM reorder/palette-drop path. Move/resize/reorder are announced via a throttled role=status region.

## Rationale

A canvas has no accessibility tree and Konva drag is pointer-only, so the canvas itself is at best a labelled region — real editing accessibility must live in DOM. SC 2.5.7 Dragging Movements (AA) is satisfied independently of keyboard: per W3C, keyboard equivalence does NOT meet 2.5.7 unless the alternative is also clickable/tappable. The numeric Inspector + steppers + reorder buttons + tap-source-then-tap-cell are the literal single-pointer alternatives W3C lists, satisfying 2.5.7, 2.1.1 Keyboard, and 4.1.2 together. Figma/FigJam 2024-2025 work is the real-world precedent that a complex spatial canvas editor can be made keyboard- and SR-operable.

## Alternatives considered

Keyboard-only equivalence (rejected: does NOT meet 2.5.7 — touch users have no keyboard); real DOM nodes overlaid on the canvas for spatial parity (viable but harder to sync/test — the separate Cells list is the lower-risk default; both can conform); relying on dnd-kit for free-form canvas dragging (rejected: dnd-kit only covers DOM sortable/drop, not Konva); Canvas Hit Regions API (rejected: deprecated/unsupported).

## Consequences

Non-trivial engineering: two surfaces kept in sync against one model, drawn focus rings, deterministic focus on add/delete, throttled announcements, RTL coordinate mirroring, and translated dnd-kit strings. dnd-kit/Konva defaults must be re-verified against pinned versions; the editing increment is a custom coordinateGetter (1px fine / grid coarse), not the 25px dnd-kit default.
