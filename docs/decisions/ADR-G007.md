# ADR-G007: Single-orchestrator "Conductor" loop replaces N independent agent terminals

- **Status:** Accepted
- **Area:** Guardrails
- **Date:** 2026-06-16
- **Source:** operator request (2026-06-16 session) — "agents working at cross purposes, on the same issue, on GPU functionality without visibility into other changes"

## Context

Multiview has been built by **multiple independent Claude Code sessions running in
separate terminals at the same time**, each spawning its own worktree lanes,
committing, and opening PRs with no shared view of the others. The repo's *design*
for parallel work is sound — `docs/development/work-schedule.md` already partitions
work into a 1-serial-integrator + 6–7-parallel-lanes fanout **by file territory** —
but nothing **executed** that partition across uncoordinated sessions. Measured state
on 2026-06-16 before this ADR:

- **198 local branches** (~145 merged-but-unpruned, ~55 unmerged) against **3 open
  PRs**; **18 worktrees** on disk (3 still `locked` by sessions whose pids had already
  exited, holding orphaned uncommitted work). Rule-9 cleanup was not happening because
  no single owner was accountable for it.
- **`crates/multiview-cli/src/pipeline.rs`** — the data-plane drive seam the work
  schedule mandates be evolved **serially** — had **7 distinct divergent in-flight
  blobs** plus an orphaned uncommitted edit. The `multiview-control` route/OpenAPI/
  `AppState` surface was being rewritten live by another lane. These collisions were
  invisible until cherry-pick/merge.
- **Duplicate effort:** the WebRTC subsystem alone had ~18 branches including two
  competing *whole-feature* attempts (`feat/webrtc-full` vs `feat/webrtc-actually-works`);
  `feat/gpu5c-placement` vs `feat/gpu5c-placement-execution` split one task across two
  branch names.
- **The shared memory was broken.** `.mcp.json` set
  `QDRANT_LOCAL_PATH=${CLAUDE_PROJECT_DIR}/.memory/qdrant`, but this Claude Code build
  does not expand `${CLAUDE_PROJECT_DIR}` in `.mcp.json` `env` values, so the qdrant
  store was created in a literal-named, untracked, non-gitignored directory and
  `qdrant-find` returned empty — i.e. sessions could not recall each other's decisions,
  a *root cause* of cross-purpose work. The embedded qdrant store is additionally
  **single-process** (one holder per clone), so concurrent terminals cannot share it.
- **The rule-21 cross-vendor review gate was untracked** across sessions: all 3 open
  PRs had zero recorded reviews.

The constraints that bound the answer are the AGENTS.md rules (8–16 worktree/PR/cleanup
discipline; 21 adversarial cross-vendor review; 32 parallel-agent territory scoping),
the technical invariants (`conventions.md` §5 — especially #1 output-clock and #10
isolation), and [ADR-G005](ADR-G005.md) (delegated merge) / [ADR-G006](ADR-G006.md)
(committed `.claude` machinery + warn-only hook).

## Decision

**One long-lived orchestrator session — the "Conductor" — is the single accountable
owner of the work loop.** It is the sole writer of the work-schedule board, the sole
integrator, the sole `memory` MCP client, and owns every PR from open to merge and the
cleanup after. It fans work out **more** broadly than the separate terminals did, using
two mechanisms, while making the repo's existing territory model actually hold.

The Conductor runs a repeating loop:

> **① PLAN** (read board + memory + open PRs; pick the next wave of dependency-ready
> tasks, each mapped to a disjoint territory) → **② ASSIGN** (write lane→territory→owner
> to the board; pick the reviewer vendor per lane) → **③ FAN OUT** (workflows and/or
> agent teams, worktree-isolated) → **④ INTEGRATE** (sole integrator; single-commit
> cherry-picks; rebuild from a clean isolated `target/` — no shared-cache fake-green) →
> **⑤ REVIEW** (dispatch each diff to a fresh, **different-vendor** reviewer; ≥1
> substantive risk required; high-risk diffs get a 3-reviewer panel) → **⑥ MERGE**
> (green deterministic CI + passing review → merge, ADR-G005) → **⑦ CLEAN** (remove the
> lane's worktree, prune its branch, `fetch && pull --ff-only` so the next wave bases on
> current HEAD) → **⑧ RECORD** (flip board checkboxes, `qdrant-store` decisions/gotchas,
> write/refresh runbooks) → **⑨** reschedule the next iteration.

Concrete shape:

- **Two fan-out modes.** *Workflow mode* (the `Workflow` tool; scripts committed under
  `.claude/workflows/`) for any sub-step that is itself a decompose→verify→synthesize
  fan-out (state mapping, review waves, mechanical sweeps, per-PR verify pipelines).
  *Team mode* (background `Agent`s + the shared `TaskList` board + `SendMessage`) for
  lane-length stateful implementation. A lane's own sub-fan-outs may themselves be
  workflows.
- **Territory partition.** The codebase is partitioned into disjoint **LANE-\*
  territories** (table below); the Conductor never runs two concurrent lanes that touch
  the same territory, and **hot shared files are serial** — `pipeline.rs`,
  `engine/{runtime,clock,drive}.rs` → LANE-CORE; `control/{routes/mod,openapi,state}.rs`
  → LANE-API. Non-owner lanes file the handler **body** and hand the **wiring** to the
  owner. The path globs live in the [`orchestrate` skill](../../.claude/skills/orchestrate/SKILL.md);
  this partition refines `work-schedule.md` §1c.
- **Cross-vendor review via Codex.** `codex` (codex-cli, verified present 2026-06-16) is
  the second vendor; Claude-authored diffs are reviewed by Codex in a fresh context
  seeing only diff + spec + checklist (rule 21). Gemini is not installed; if added it
  joins the rotation. High-risk diffs (engine/concurrency/auth/migration) escalate to a
  3-reviewer panel and the operator is notified. The review and its ≥1 substantive risk
  statement are recorded on the PR.
- **Substrate fix.** `.mcp.json` uses the relative path `.memory/qdrant` (no variable
  expansion); `.gitignore` guards both `.memory/` and the stray `${CLAUDE_PROJECT_DIR}/`
  dir; the runbook is corrected (see [memory-mcp runbook](../runbooks/memory-mcp.md)).
- **Autonomy.** Per operator directive 2026-06-16 the loop runs **fully self-paced**
  (ScheduleWakeup-driven), operator-interruptible, with the operator retaining ultimate
  override (ADR-G005).

### Territory partition (refines work-schedule.md §1c)

| Territory | Owns (representative paths) | Serial? |
| --- | --- | --- |
| **LANE-CORE** | `multiview-cli/src/{pipeline,sink,run,control}.rs`, `multiview-engine/src/{runtime,drive,clock}.rs`, `multiview-events/src/event.rs`, `multiview-config/src/schema.rs` | **Serial — one owner only** |
| **LANE-API** | `multiview-control/src/{routes/mod,openapi,openapi_schemas,asyncapi,state,lib}.rs`, `docs/api/openapi.json`, auth/session/RBAC | **Serial — one owner only** |
| **LANE-WRTC** | `multiview-webrtc/**`, `multiview-preview/src/whep*`, `control/src/routes/{whip,whep_serve}.rs`, WHEP/WHIP SPA (coordinate w/ LANE-WEB) | one owner |
| **LANE-IN** | `multiview-input/**`, `multiview-rist-sys/**` (file body; hand `pipeline.rs` wiring to LANE-CORE) | one owner |
| **LANE-PRV** | `multiview-preview/**` (taps, encoder pool, MJPEG/snapshot; WHEP transport → LANE-WRTC) | one owner |
| **LANE-ENG** | `multiview-engine/**` *excluding* runtime/clock/drive (PTS norm, PTP/NTP, HA, admission), `multiview-hal/src/load.rs` | one owner |
| **LANE-GPU** | `multiview-compositor/**`, `multiview-framestore/**`, `multiview-ffmpeg/**`, `multiview-hal/src/select.rs` | one owner |
| **LANE-AUDIO** | `multiview-audio/**` (decode/resample/mix/route/R128, AES67/ST2110-30), `multiview-overlay/**` | one owner |
| **LANE-BCAST** | `control/src/{nmos,is07}*`, `multiview-output/**` HLS/LL-HLS/RTSP/NDI/RTMP/SRT (mux seam → LANE-CORE) | one owner |
| **LANE-WEB** | `web/src/**`, `web/public/**`, `web/package*.json` (consumes LANE-API's OpenAPI) | one owner |
| **LANE-DEVICES** | managed devices/display-out: zowietek, display-kms/scanout, sync groups, cast, node-enroll, `deploy/**` node assets | one owner |
| **LANE-CONSPECT** | `multiview-licence/**`, `multiview-mesh/**`, conspect heartbeat/lease/enforcement/audit | one owner |
| **LANE-GOV** | `.claude/**`, `docs/{decisions,research,runbooks,development/work-schedule.md}`, `.github/workflows/**`, build pins | one owner |

## Rationale

- A single brain is the only thing that can *enforce* territory disjointness: collisions
  on `pipeline.rs` cannot surface at merge if two colliding lanes are never **assigned**
  at once. The 7-blob `pipeline.rs` situation is impossible under one assigner.
- The embedded qdrant memory is **single-process by construction** — it *wants* exactly
  one long-lived client. One Conductor dissolves the lock contention and gives every wave
  real recall, attacking a root cause of duplicate work.
- It restores the rules that were silently lapsing — rule-9 cleanup, rule-14 PR-to-merge
  ownership, rule-21 review tracking — by making one party accountable for them, without
  weakening any gate (rule 7: autonomy never bypasses quality).
- It increases parallelism rather than reducing it: workflows fan out dozens of agents
  per wave under one coordinator, and lanes still run concurrently across disjoint
  territories — the difference is they no longer collide or duplicate.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| **Keep N independent terminals** (status quo) | Produced the 198-branch sprawl, 7-blob `pipeline.rs`, duplicate WebRTC attempts, broken shared recall, and untracked review gate this ADR exists to fix. No party is accountable for integration or cleanup. |
| **N terminals + a locking protocol** (e.g. a territory-claim file) | A distributed lock among uncoordinated sessions is exactly what the single-process memory and the work-schedule board already are, and they were ignored. Adds coordination overhead without an accountable owner; stale locks (dead pids still `locked`) recur. |
| **Pure CI-bot automation** (no orchestrator agent) | Cannot do the design/decompose/adversarial-review judgement; reduces to mechanical merge, which is the part already delegated (ADR-G005). The hard part is planning the wave and verifying the work. |
| **One terminal, no fan-out** (serial single agent) | Throughput collapses; the whole point is broad parallelism. The Conductor keeps the fan-out and adds the coordinator. |

## Consequences

- **Easier:** no merge-time collisions on hot files; real cross-session recall; one
  accountable owner for CI-to-merge and cleanup; the branch/worktree count stays bounded;
  the rule-21 gate is always applied and recorded.
- **Harder / committed to maintain:** the Conductor is a **single point of throughput** —
  if it stalls, the loop stalls (mitigated: fully self-paced ScheduleWakeup, operator can
  interrupt/resume, and waves are independent so a failed wave doesn't block others). The
  Conductor must **track lane authorship** to pick a different-vendor reviewer, and must
  **rebase stale lanes onto current HEAD** before integrating (every in-flight lane on
  2026-06-16 sat on a stale base). Build-cache discipline (rule 11 — clean isolated
  `target/` before trusting green) and the rule-10 `/tmp` target-dir ban bind every wave.
- **Invariants:** the loop's INTEGRATE/REVIEW steps must keep invariant **#1
  (output-clock)** and **#10 (isolation)** as blocking gates for any engine/data-plane
  wave (chaos/soak + mutation bars before merge); these never relax under self-pacing.
- **Operator authority retained** (ADR-G005): irreversible/outward-facing actions beyond
  routine merge — force-pushing `main`, deleting infrastructure, public releases, external
  comms — are surfaced to the operator, not done silently.
