---
name: adr
description: Record an architecture decision as an ADR in docs/decisions/. Use when a decision constrains future changes (technology choice, data model, protocol, security posture, dependency) — required at R3, at R2 only if it constrains, never at R0/R1. Write it before or alongside the implementing commit.
---

# Writing an ADR

**Write an ADR when a decision constrains future changes — not for every change.**
Required at R3; at R2 only when the decision genuinely binds what comes later;
never at R0/R1. Classify with `scripts/classify.sh`; the contract is
[engineering.md](../../../docs/standards/engineering.md) Part G. ADRs are the
**why**; the operational **how** lives in `docs/runbooks/`.

In this repo ADRs live in [`docs/decisions/`](../../../docs/decisions/) (NOT
`docs/adr/`; [ADR-G006](../../../docs/decisions/ADR-G006.md)).
[`docs/decisions/README.md`](../../../docs/decisions/README.md) is
the catalog. [`docs/architecture/conventions.md`](../../../docs/architecture/conventions.md)
remains the canonical source of truth — an ADR may not contradict it.

## Numbering & prefixes

ADRs use an area-prefixed scheme; pick the prefix for your area and the next free
number within it:

| Prefix | Area | Prefix | Area |
|--------|------|--------|------|
| `ADR-NNNN` | Core engine / cross-cutting | `ADR-P*` | Preview |
| `ADR-C*` | Color | `ADR-R*` | Resilience & A/V |
| `ADR-DC*` | Dev container | `ADR-RT*` | Realtime API |
| `ADR-E*` | Efficiency | `ADR-T*` | Streaming / timing |
| `ADR-G*` | Engineering guardrails / governance | `ADR-W*` | Web / API stack |
| `ADR-I*` | Implementation build-out | `ADR-M*` | Management |
| `ADR-MV*` | Broadcast multiviewer | | |

Find the next number: `ls docs/decisions/ | grep '^ADR-<PREFIX>'`.

## Procedure

1. Copy [`docs/decisions/TEMPLATE.md`](../../../docs/decisions/TEMPLATE.md) to
   `docs/decisions/ADR-<PREFIX><NNNN>.md`.
2. Fill every section: Status, Area, Date (absolute), Source, Context, Decision,
   Rationale, **Alternatives considered** (name real alternatives + the specific
   reason each was rejected — "didn't fit" is not a reason), Consequences.
3. Status starts at `Accepted` when the decision is real and being implemented;
   use `Proposed` only for a decision derived from a brief that is not yet built.
   A reversal edits the old ADR's status to `Superseded by ADR-<id>` in the **same
   commit** that adds the superseding ADR.
4. Add a one-line entry to `docs/decisions/README.md`.
5. Commit the ADR with the work it justifies, or as its own `docs(adr):` commit if
   the decision precedes implementation.

## Quality bar

- Present tense, factual, self-contained — a reader gets the full picture without
  the chat transcript that produced it.
- Numbers and names, not vibes: versions, benchmarks, prices, URLs, ADR/brief
  cross-links.
- An ADR that describes behaviour the repo doesn't have yet is aspirational
  documentation, and that is a defect — write `Accepted` ADRs when the decision is
  real, and mark genuinely-future ones `Proposed`.
