# ADR-<PREFIX><NNNN>: <decision title>

- **Status:** Accepted <!-- Proposed | Accepted | Superseded by ADR-<id> -->
- **Area:** <Core engine | Color | Efficiency | Guardrails | Management | Preview | Resilience & A/V | Realtime API | Streaming/Timing | Web/API | Dev container | …>
- **Date:** YYYY-MM-DD
- **Source:** <operator request | research brief link | agent session | incident>

## Context

What problem or pressure forced a decision. The constraints that bound the answer
(the canonical invariants in [`../architecture/conventions.md`](../architecture/conventions.md) §5,
the AGENTS.md rules, licensing, platform, hardware, deadlines). State what is true
today, not what we wish were true (rule 27).

## Decision

The decision in one or two sentences, present tense. Then the concrete shape:
crate(s)/module(s) touched, feature flags, versions, configuration, commands,
file paths.

## Rationale

Why this option over the others — the load-bearing reasons, with numbers/names
where they exist.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| <name> | <specific reason — not "didn't fit"> |

## Consequences

What becomes easier, what becomes harder, what we are now committed to
maintaining. Include operational consequences (licensing impact, CI cost, upgrade
cadence, lock-in) and any invariant (#1 output-clock, #10 isolation, …) the
change touches.
