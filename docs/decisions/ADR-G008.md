# ADR-G008: Gates scale with blast radius — replace the 42-rule prose contract with a change-class matrix

- **Status:** Accepted
- **Area:** Engineering guardrails / governance
- **Date:** 2026-07-27
- **Source:** Operator directive (Troy Kelly, repository owner) — the agent-instruction surface had become the dominant cost of doing work; cut it without reducing delivered quality.
- **Amends:** [ADR-G006](ADR-G006.md) §7 (which adopted the 42-rule contract as authoritative). Reconciles with, and does not weaken, [ADR-G001](ADR-G001.md)–[ADR-G005](ADR-G005.md) and [ADR-G007](ADR-G007.md).

## Context

[ADR-G006](ADR-G006.md) adopted a generic 42-rule working contract into `AGENTS.md` and kept every
Multiview-specific section alongside it. That was the right call at the time — it installed real
enforcement where there had been prose. Eighteen months of accretion later, measured on
2026-07-27:

- `AGENTS.md` (32,725 B) + `CLAUDE.md` (19,232 B) = **51,957 B auto-loaded into every session**,
  before any file is read. The total governance surface across 37 files was **235,409 B**.
- The mandated pre-read (rule 30 + the `CLAUDE.md` §6 brief table) put a **71,705 B floor** on the
  cheapest possible change, and **169–221 KB** on an ordinary crate change.
- The cross-vendor review carried a **96,464 B fixed context floor per low-risk item** — of which
  the review script contributed 1,993 B and `AGENTS.md` contributed 65,450 B, because it is billed
  twice (once by the Claude harness, once by Codex's own project-doc load). None of it was gated by
  path, diff size, or risk.
- `AGENTS.md` had reached **43 bytes** of Codex's 32,768 B `project_doc_max_bytes` default. Growth
  past that would have silently truncated the reviewer's copy of the contract.
- Every change paid the same ceremony regardless of blast radius: a two-line ADR edit and an
  outbound-SSRF fix owed the same issue, plan, test-first, full-suite and review obligations.

The duplication was also drifting into outright error. `AGENTS.md` §C omitted `multiview-webrtc`,
§E omitted the `rist`/`rist-stats`/`native` features, and §H specified WCAG 2.1 AA where
`conventions.md` — the source of truth — specifies 2.2 AA.

[ADR-G004](ADR-G004.md) had already recorded the principle this ADR acts on: *"`AGENTS.md`/`CLAUDE.md`
must stay concise (bloat causes agents to ignore instructions), so deterministic must-happen actions
live in hooks/CI, not prose."*

## Decision

**Gates scale with blast radius, not with change count.** Safety controls stay unconditional —
they are nearly free per change because CI and platform config enforce them, not agent labour.
Ceremony scales down hard.

1. The 42 numbered rules are retired. `AGENTS.md` becomes a **router** (84 lines): what this is,
   the repo-specific invariants that cause real harm if broken, the commands, the class table, and
   where things live. `CLAUDE.md` (21 lines) keeps its `@AGENTS.md` import and adds only
   Claude-Code specifics.
2. [`docs/standards/engineering.md`](../standards/engineering.md) is the process contract: the
   **R0–R3 gate matrix**, mechanical classification, unconditional safety invariants, evidence
   rules, delegation and model routing, context discipline, documentation proportionality, and
   exceptions.
3. Classification is mechanical. [`scripts/classify.sh`](../../scripts/classify.sh) reads the diff,
   applies a path map plus an R0 content test, and prints the class and the gates it owes. **The
   class is a floor an author may raise and may never lower.** Its R0 test reuses the prose
   allowlist from [ADR-0046](ADR-0046.md) so the classifier and CI's `changes` job cannot disagree.
   A new advisory `change class` CI job reports it on every PR.
4. Technical content is not summarised in the root files any more — it is read from its single home:
   `conventions.md` (names, crate map, features, invariants, licensing), the new
   [`data-plane-safety.md`](../architecture/data-plane-safety.md) (the eight engine/FFI safety
   rules, previously `CLAUDE.md` §7, **with rule numbers §1–§8 preserved** so the ~133 existing
   citations in source comments and docs still resolve), `codebase-map.md` (crate map + which brief
   to read), and `agent-guardrails.md` (retained at its path, rewritten as the toolchain-forensics
   reference).
5. `.github/CODEOWNERS` is created. The classifier, the standard, the agent instructions, CI, and
   the trust-boundary and legal paths route to the operator — which is what makes "the agent cannot
   lower its own class" a mechanism rather than a promise, to the extent branch protection allows
   (see Consequences).

### What this ADR explicitly does NOT change

- **[ADR-G005](ADR-G005.md) stands unchanged.** The agent remains the delegated approver *and*
  merger for routine changes; the cross-vendor review remains mandatory, is never self-performed by
  the authoring vendor, and still gates the merge; the operator retains ultimate authority. R2
  therefore does **not** reinstate a per-PR human approval gate — doing so would contradict an
  Accepted decision. R3 covers the outward-facing actions G005 reserves to the operator.
- **[ADR-G006](ADR-G006.md)'s mechanisms stand.** The worktree hook stays **warn-only** (operator
  choice); ADRs stay in `docs/decisions/`; `.claude/` stays committed.
- **[ADR-G007](ADR-G007.md) stands.** The 9-step Conductor wave remains the live delivery loop; the
  standard cites it rather than replacing it.
- **[ADR-G002](ADR-G002.md)'s mutation-testing mandate is not withdrawn** — but it is now recorded
  honestly as *not wired* (see Consequences).
- No safety test was modified, no CI job was renamed, removed or split, and no branch-protection
  change was attempted.

## Rationale

The intent behind the 42 rules was right: quality, tested, documented, reviewed code. The
implementation taxed every change equally regardless of blast radius, and the tax was paid in the
one resource that determines output quality — context. Oversized context is not merely expensive;
it buries the instructions that matter and dilutes review attention.

Blast radius is the correct axis because it is the one that predicts harm. It is also mechanically
observable from a diff, which lets the decision be made by a script rather than argued by the agent.

Keying on **paths and not commit type** is load-bearing: this repository has a dense population of
`docs(...)`-typed commits whose entire diff is doc comments inside `crates/**/*.rs`. A type-keyed
classifier would have dropped 32 of 37 checks on each of them.

The data plane is R2 rather than R1 because bulletproof output from bad inputs *is* the product;
`conventions.md` invariants #1–#10 live in those crates. The savings therefore come from collapsing
the context floors — not from removing review from the media pipeline, which G005/G007 forbid and
which would be a real safety regression.

## Alternatives considered

- **Keep the 42 rules, just trim the prose.** Rejected: the cost is structural (everything loaded
  always, every gate applied always), not editorial. Trimming wording would not have moved the
  71.7 KB pre-read floor or the 96 KB review floor.
- **Delete `agent-guardrails.md` and fold it into the new standard.** Rejected: 14 documents link to
  it, and `clippy.toml` and `web/eslint.config.js` cite it by path as their rationale. Deleting it
  fails `docs-sanity` on every PR until all referrers are fixed. Rewriting in place costs nothing.
- **Move the safety rules without preserving `§1`–`§8`.** Rejected: ~133 citations across 76 files
  (37 of them in `.rs` FFI-safety comments) reference them by number. Preserving the numbering keeps
  every one resolvable without touching source.
- **A single R1/R2 split.** Rejected: R0 is a large, genuinely free category here (docs and ADRs are
  a substantial share of commits), and R3 is needed to keep the 3-lens panel on trust boundaries.
- **Make classification advisory.** Rejected: an advisory class is one the agent argues with.

## Consequences

- Session-load instruction bytes fall from **51,957 → 7,607 B (−85%)**. Total governance surface
  falls from **235,409 → ~176,000 B**. The review context floor falls from ~96 KB to ~13 KB per
  low-risk item, because the floor was overwhelmingly the root files.
- The `AGENTS.md`-vs-Codex-truncation hazard is gone: 6,003 B against a 32,768 B ceiling.
- `AGENTS.md` is 6,003 B, over the 4 KB target it was written to. The overage is the
  "Non-negotiable" list — 20 repo-specific invariants — plus the class and location tables. Deleting
  an invariant to hit a byte target would defeat the purpose; the ceiling yields.
- **Newly recorded as documented-but-not-enforced** (none of these are regressions introduced here;
  all pre-date this ADR and are now stated instead of implied): no `cargo mutants` job exists in any
  workflow despite ADR-G002 and ADR-G006 asserting one; no self-hosted or GPU-tagged runner exists,
  so the SSIM/PSNR parity assertions compile and then skip on every run; `main` has no branch
  protection and no rulesets, so CI green and `CODEOWNERS` are advisory rather than blocking; the
  pre-commit `gitleaks` hook silently no-ops when the binary is absent; `clippy::undocumented_unsafe_blocks`
  is not enabled, so `// SAFETY:` is convention-only; no DCO check exists despite `CONTRIBUTING.md`
  previously claiming one; and no CI job regenerates or diffs the two generated TypeScript files.
- **CODEOWNERS routes review but cannot block it** until branch protection or a ruleset requiring
  code-owner review is enabled. Enabling that is the single highest-value follow-up and is the
  operator's to make.
- ~241 bare `rule N` citations remain across the repository (147 in `.md`, 87 in `.rs`). Those in the
  governance surface and in `ci.yml` were rewritten here; the rest are stale pointers that no gate
  reads. Fixing the `.rs` ones would mean touching 87 source files and is deliberately out of scope
  for a governance change.
- Six FFI crates hand-copy the workspace lint set with no drift check — unchanged by this ADR, now
  documented in `agent-guardrails.md` §A.1.
