# ADR-W011: Realtime status/tally/alarms — no color alone + disciplined aria-live

- **Status:** Proposed
- **Area:** Accessibility & Internationalization
- **Date:** 2026-06-02
- **Source:** [web/accessibility.md](../web/accessibility.md)

## Decision

Every realtime state (tally red/green/amber, alarm severity, connection, meters) is triple-encoded — color + shape/icon + text label — and carries its state in the accessible name; the CVD-safe Wong/Okabe-Ito palette is the hue floor, contrast-verified against both dark and light themes. Announcements use pre-mounted role=status (polite) for ~90% of updates and role=alert (assertive) only for critical alarm raises, via one global announcer that never recreates the node; per-tile updates are debounced/coalesced (~1-2s, tunable) into summaries with aria-busy while batching; alarm history is a role=log (polite, non-atomic). Audio meters are silent native <meter>/role=meter gauges with aria-valuetext on a focusable wrapper — never streamed over aria-live — announcing only thresholds (clip assertive, silence polite). Meters, pulses, and transitions are gated behind prefers-reduced-motion; nothing flashes >3x/s.

## Rationale

Tally/severity by color alone fails SC 1.4.1 Use of Color (A). SC 4.1.3 Status Messages (AA) requires programmatic determinability without focus change — but high-rate meters streamed over aria-live flood AT, so meters are document-structure gauges (not a live role) read on demand, and only meaningful threshold/lifecycle events are spoken. Pre-mounted regions are required because a region created and populated together usually does not announce. Borders/icons/meter fills need 1.4.11 (3:1) and label text 1.4.3 (4.5:1), both themes.

## Alternatives considered

Color-only tally/severity (rejected: 1.4.1 failure + colorblindness trap); streaming meter values over aria-live (rejected: floods AT; role=meter is not a live region and MDN says meter is not for unbounded values); assertive for most updates (rejected: disorienting, implicates 2.2.4); creating a fresh live region per event (rejected: SRs lose registration).

## Consequences

Needs a global announcer/message-bus, debounce/coalesce tuning, and a CVD palette validated per theme. Live announcements are transient, so critical alarm state must also persist visibly (card + log + name). aria-busy, debounce cadence, and role=log 'new entries only' behaviour vary by screen reader and must be validated against real NVDA/JAWS/VoiceOver.
