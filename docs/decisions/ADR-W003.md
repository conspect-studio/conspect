# ADR-W003: Frontend stack: React 19 + TS + Vite + shadcn/ui + TanStack Query

- **Status:** Proposed
- **Area:** Web/API Stack
- **Date:** 2026-06-02
- **Source brief:** [web-api-stack.md](../research/web-api-stack.md)

## Decision

Build the SPA on React 19 + TypeScript + Vite with shadcn/ui (Radix + Tailwind v4) as the design system and TanStack Query (+ TanStack Table) for server state, typed against the generated OpenAPI client.

## Rationale

Deepest ecosystem for the component- and canvas-heavy editor; Radix delivers WCAG/WAI-ARIA + keyboard/focus for free; Tailwind v4 OKLCH tokens + .dark override give coherent themeable light/dark; TanStack Query handles caching/optimistic updates and integrates with SSE.

## Alternatives considered

SvelteKit (great DX, thinner DnD/canvas/component ecosystem); Leptos/Dioxus (all-Rust appeal but immature UI/a11y/editor tooling); MUI/Ant Design (heavier, harder to make bespoke).

## Consequences

Frontend is a JS/TS toolchain alongside Rust, requiring a Vite build step wired into cargo build; careless shadcn customization can strip Radix a11y wiring — preserve it and test with a screen reader.
