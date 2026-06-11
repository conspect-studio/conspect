# ADR-W004: Layout-editor library: react-konva (canvas) + dnd-kit

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Implement the visual multiview/template editor on react-konva (canvas) with snap-to-grid, a Transformer for resize/rotate, and layer-based z-order, using dnd-kit for the accessible source palette and reorderable lists.

## Rationale

The product is a GPU compositor (broadcast outputs) needing free-form placement, overlap, z-order and rotation — Konva provides exactly these primitives; dnd-kit gives accessible mouse/keyboard/touch drag for the surrounding UI.

## Alternatives considered

react-grid-layout (verified-refuted: no rotation, no native z-index, overlap is a workaround — only fits a strict non-overlapping grid); Fabric.js (weaker React bindings); react-rnd (no structured editor model); commercial SDK (Polotno/IMG.LY) to shortcut months of build.

## Consequences

A production-grade canvas editor is months of effort; must confirm the engine compositor actually supports overlap/z-order/rotation/sub-pixel placement (expected yes) before committing the canvas approach.
