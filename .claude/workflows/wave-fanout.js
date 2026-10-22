// wave-fanout — launch one wave of lane work across DISJOINT file territories, each in
// its own isolated worktree meeting its own class's gates, returning committed work + an
// opened PR for the orchestrator to review (review-wave) and merge. Each lane classifies
// itself with `scripts/classify.sh`; the class is a floor it may raise, never lower.
// Territories must be disjoint and hot shared files (pipeline.rs,
// engine/{runtime,clock,drive}.rs, control/{routes/mod,openapi,state}.rs) must be
// assigned to a single owner lane — the orchestrator guarantees this in its ASSIGN
// step before calling. Usage:
//   Workflow({ name: 'wave-fanout', args: { lanes: [
//     { id: 'gpu-hwdefect', territory: 'LANE-GPU', item: 'HW-DEFECT-A', prompt: '...' , highRisk: false },
//     ... ] } })
export const meta = {
  name: 'wave-fanout',
  description: 'Run one wave of lane implementation across disjoint territories in parallel: each lane classifies its change (scripts/classify.sh), works in an isolated worktree, runs the gates that class owes, and opens a PR. Returns per-lane results (class, branch, commits, PR, gate status) for the orchestrator to review and merge. Disjoint-territory assignment is the orchestrator’s responsibility before calling.',
  whenToUse: 'The FAN OUT step of a Conductor wave once territories are assigned and dependency-ready.',
  phases: [{ title: 'Implement' }],
}

const LANE_RESULT = { type: 'object', additionalProperties: false,
  required: ['id', 'territory', 'status', 'branch', 'commits', 'localGate', 'summary'],
  properties: {
    id: { type: 'string' }, territory: { type: 'string' },
    status: { type: 'string', description: 'opened-pr | committed-no-pr | blocked | abandoned' },
    class: { type: 'string', description: 'R0 | R1 | R2 | R3 — what `scripts/classify.sh --quiet` printed for this diff (raised if the lane escalated)' },
    branch: { type: 'string' }, prNumber: { type: 'string' },
    commits: { type: 'array', items: { type: 'string' }, description: 'short SHAs in order, the red test commit first where the class owes one' },
    localGate: { type: 'string', description: 'exact result of the gates this class owed — the commands run, pass/fail, and any failing output' },
    filesTouched: { type: 'array', items: { type: 'string' } },
    summary: { type: 'string' }, blockers: { type: 'string' },
  } }

// Gates scale with blast radius, so the lane derives them rather than paying a fixed
// ceremony: `scripts/classify.sh` reads the diff and prints the class + what it owes.
const RULES = [
  'NON-NEGOTIABLE: stay strictly within your territory paths; if you need a change to a hot shared file owned by another lane (pipeline.rs, engine/{runtime,clock,drive}.rs, control/{routes/mod,openapi,state}.rs) you do NOT own, implement only your body and describe the required wiring in blockers for the owner — do NOT edit it.',
  'GATES SCALE WITH BLAST RADIUS. Run `scripts/classify.sh` in your lane FIRST and again before the PR: it prints your class and exactly which gates that class owes. The class is a floor — raise it when you know something the paths cannot show, never lower it (contract: docs/standards/engineering.md).',
  'R0 (prose only, no executable or spec effect) owes CI only. R1 owes a failing test first for a behavioural change plus the focused crate suite (cargo test -p multiview-<crate>). R2/R3 owe a failing test first plus the FULL local gate before the PR: cargo fmt --all -- --check; cargo clippy --locked --workspace --all-targets -- -D warnings; cargo test --locked --workspace; cargo deny check (if dependencies changed) — plus web/ lint+typecheck+build if web/ changed.',
  'Where your class owes a failing test: write it FIRST, run it, paste the actual red output, commit it as its own commit (Conventional Commits + the Co-Authored-By trailer), THEN implement to green without touching the test. Never weaken, skip, #[ignore] or delete a test to make a build pass. Absolute typing, no unwrap/expect/panic in non-test code, no #[allow] without an inline justification.',
  'A change that risks invariant #1 (output-clock) or #10 (isolation) is R3 whatever its path: stop, write the design note, name it in blockers, and expect the 3-lens panel + a chaos/soak test before it can land.',
  'Build in your OWN isolated target/ (never /tmp, never override CARGO_TARGET_DIR). If the gates your class owes are not green, do NOT open the PR — return status=blocked with the failing output.',
].join(' ')

function runLane(l) {
  return agent(
    `You own lane "${l.id}" (territory ${l.territory}) in the Multiview repo. Base your work on current origin/main (rebase if your worktree is on a stale base).\n\n` +
    `TASK: ${l.item}\n${l.prompt}\n\n` +
    `${RULES}\n\n` +
    `When green: push your branch and open a PR (\`gh pr create\`) with a Conventional-Commit title, a body explaining the change + how it was tested + the invariants re-asserted, ending with the "Generated with Claude Code" line. Return the structured result (the class classify.sh printed, branch, ordered commit SHAs, the PR number, the exact gate result, files touched, and any wiring you handed off to a hot-file owner in blockers). The orchestrator will run the cross-vendor review and own the PR to merge — do NOT merge it yourself.`,
    { label: `lane:${l.id}`, phase: 'Implement', schema: LANE_RESULT, isolation: 'worktree',
      ...(l.model ? { model: l.model } : {}), ...(l.highRisk ? { effort: 'high' } : {}) })
}

phase('Implement')
const lanes = (args && args.lanes) || []
const results = await parallel(lanes.map((l) => () => runLane(l)))
return { results: results.filter(Boolean) }
