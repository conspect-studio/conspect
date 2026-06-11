# ADR-W009: Target WCAG 2.2 AA across the management web app

- **Status:** Proposed
- **Area:** Accessibility & Internationalization
- **Date:** 2026-06-02
- **Source:** [web/accessibility.md](../web/accessibility.md)

## Decision

Commit the management web app to WCAG 2.1 and 2.2 Level AA as a release gate, building on Radix/shadcn for the WAI-ARIA patterns and enforcing conformance with a layered test stack (eslint-plugin-jsx-a11y, jest-axe/vitest-axe on components, @axe-core/playwright on routes, keyboard-only E2E, and a manual screen-reader matrix of NVDA+Firefox/Chrome, JAWS+Chrome, VoiceOver+Safari macOS and iOS) wired as a CI gate that fails on new violations.

## Rationale

WCAG 2.2 adds the AA criteria most relevant to this console (2.4.11 Focus Not Obscured, 2.5.7 Dragging Movements, 2.5.8 Target Size, 3.3.8 Accessible Authentication) over 2.1. Automated tooling detects only ~57% of issues by volume (Deque) — not 57% of criteria — so manual SR testing is mandatory; Radix ships the APG patterns but explicitly does NOT guarantee WCAG conformance, so the app must still supply names, content, and the test gate.

## Alternatives considered

WCAG 2.1 AA only (rejected: omits 2.5.7/2.5.8 which govern the drag-heavy editor and dense multiviewer); AAA target (rejected: not all AAA SCs are achievable/appropriate for a realtime broadcast tool); automated-only testing (rejected: misses ~43% of issues incl. SR experience).

## Consequences

Adds CI tooling, a per-milestone manual SR pass, and design constraints (visible focus, target size, no color-alone). aria-busy reliability, live-region cadence, and role=log behaviour vary by screen reader and must be validated against real NVDA/JAWS/VoiceOver. Defaults quoted for dnd-kit/Radix must be re-verified against pinned versions once a lockfile exists.
