// orient — map in-flight Multiview work (lanes, branches, PRs, hot-file collisions,
// coordination machinery) into one state-of-the-world for the Conductor's PLAN step.
// Read-only. Usage: Workflow({ name: 'orient', args: { openPrs: [170, 172] } })
// args.openPrs is optional; if omitted the PR reader discovers open PRs via gh.
export const meta = {
  name: 'orient',
  description: 'Read-only map of in-flight work: worktree lanes, branch sprawl, open PRs, hot-file collisions, and coordination machinery — synthesized into one actionable state-of-the-world for the Conductor.',
  whenToUse: 'At the start of a Conductor session or wave, to recover what is in flight and where lanes collide before assigning work.',
  phases: [
    { title: 'Map', detail: 'parallel readers: lanes, branches, PRs, hot-file collisions, governance' },
    { title: 'Synthesize', detail: 'cross-reference into ready-to-merge / collisions / duplicates / prunable / risks' },
  ],
}

const STR_ARR = { type: 'array', items: { type: 'string' } }

const LANES_SCHEMA = { type: 'object', additionalProperties: false, required: ['lanes', 'notes'], properties: {
  lanes: { type: 'array', items: { type: 'object', additionalProperties: false,
    required: ['path', 'branch', 'territory', 'commitsAhead', 'dirty', 'locked', 'staleBase', 'summary'],
    properties: { path: { type: 'string' }, branch: { type: 'string' }, territory: { type: 'string' },
      commitsAhead: { type: 'integer' }, dirty: { type: 'boolean' }, locked: { type: 'boolean' },
      staleBase: { type: 'boolean' }, summary: { type: 'string' } } } },
  notes: { type: 'string' } } }

const BRANCHES_SCHEMA = { type: 'object', additionalProperties: false, required: ['total', 'mergedPrunable', 'topicClusters', 'staleCandidates'], properties: {
  total: { type: 'integer' }, mergedPrunable: STR_ARR,
  topicClusters: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['topic', 'branches', 'duplicationRisk'],
    properties: { topic: { type: 'string' }, branches: STR_ARR, duplicationRisk: { type: 'string' } } } },
  staleCandidates: STR_ARR } }

const PRS_SCHEMA = { type: 'object', additionalProperties: false, required: ['prs'], properties: {
  prs: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['number', 'title', 'territory', 'ciState', 'mergeable', 'blocker', 'recommendedAction'],
    properties: { number: { type: 'integer' }, title: { type: 'string' }, territory: { type: 'string' },
      ciState: { type: 'string' }, mergeable: { type: 'string' }, blocker: { type: 'string' }, recommendedAction: { type: 'string' } } } } } }

const COLLISION_SCHEMA = { type: 'object', additionalProperties: false, required: ['fileOverlaps', 'hotPathRisks'], properties: {
  fileOverlaps: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['file', 'refs', 'severity'],
    properties: { file: { type: 'string' }, refs: STR_ARR, severity: { type: 'string' } } } },
  hotPathRisks: { type: 'string' } } }

const GOV_SCHEMA = { type: 'object', additionalProperties: false, required: ['boardState', 'readyWork', 'risks'], properties: {
  boardState: { type: 'string' }, readyWork: STR_ARR, risks: { type: 'string' } } }

phase('Map')
const prHint = args && args.openPrs ? `Known open PRs: ${JSON.stringify(args.openPrs)}.` : 'Discover open PRs via `gh pr list --state open`.'
const [lanes, branches, prs, collisions, gov] = await parallel([
  () => agent(
    `Map every worktree lane in the Multiview repo. Run \`git worktree list --porcelain\` from the current checkout — it reports every lane of the repo whichever one you are in, so never hard-code a repo path. For each lane under .claude/worktrees/ (skip the root and any main-baseline): branch; locked? (porcelain 'locked' line); commits ahead of origin/main (\`git -C <p> rev-list --count origin/main..HEAD\`); dirty (\`git -C <p> status --porcelain\` non-empty); staleBase (its merge-base with origin/main is well behind origin/main); the LANE-* territory it maps to (see .claude/skills/orchestrate/SKILL.md); and a one-line summary of what it is doing. Use \`git -C <path>\`, never cd. Read-only.`,
    { label: 'lanes', phase: 'Map', schema: LANES_SCHEMA }),
  () => agent(
    `Triage local branches in the Multiview repo (branch refs are shared across every worktree — run git from the current checkout). origin/main is the base. mergedPrunable = \`git branch --merged origin/main\` minus main and minus any salvage/* branch. topicClusters = group the rest by prefix/keyword (webrtc, gpu, conspect, ndi, rist, ship/dev, ci, docs…); for clusters >1 judge duplicationRisk. staleCandidates = branches with tip committerdate older than 5 days and no open PR. Read-only; concrete branch names.`,
    { label: 'branches', phase: 'Map', schema: BRANCHES_SCHEMA }),
  () => agent(
    `Analyze open PRs in the Multiview repo. ${prHint} For each: \`gh pr view <n>\`, \`gh pr checks <n>\`, \`gh pr view <n> --json mergeable,mergeStateStatus\`. Report territory, CI state (note CANCELLED/failed legs and whether a bare re-run fixes it vs a real regression), mergeable/mergeStateStatus, the concrete merge blocker, and a recommended next action. Read-only — do not merge.`,
    { label: 'prs', phase: 'Map', schema: PRS_SCHEMA }),
  () => agent(
    `Detect cross-lane file collisions in the Multiview repo — the core failure mode is two in-flight refs editing the same hot file. For each open PR and each worktree lane and each fresh feature branch, get changed files vs origin/main (\`gh pr diff <n> --name-only\` or \`git diff --name-only origin/main...<ref>\`). Build fileOverlaps: any file edited by >1 ref → {file, refs, severity} (severity high for serial hot files pipeline.rs / engine {runtime,clock,drive}.rs / control {routes/mod,openapi,state}.rs). hotPathRisks: any in-flight change risking invariant #1 (output clock) or #10 (isolation) — either risk makes that change R3 (3-lens panel + chaos/soak + operator approval), so name it explicitly. Read-only.`,
    { label: 'collisions', phase: 'Map', schema: COLLISION_SCHEMA }),
  () => agent(
    `Summarize the Multiview work board for the Conductor. Search (rg, do NOT read whole — it is ~400 KB) docs/development/work-schedule.md: boardState (how many items, streams, how status is tracked); readyWork (a list of dependency-ready items: status [ ] or [~] whose deps appear satisfied, as "ID — title"); risks (top coordination risks visible right now). Also glance at qdrant-find for recent Conductor decisions. Read-only.`,
    { label: 'governance', phase: 'Map', schema: GOV_SCHEMA }),
])

phase('Synthesize')
const synthesis = await agent(
  `Synthesis step of a Conductor orientation. Cross-reference these findings into one actionable state-of-the-world.\n\n` +
  `LANES:\n${JSON.stringify(lanes)}\n\nBRANCHES:\n${JSON.stringify(branches)}\n\nPRS:\n${JSON.stringify(prs)}\n\nCOLLISIONS:\n${JSON.stringify(collisions)}\n\nGOVERNANCE:\n${JSON.stringify(gov)}\n\n` +
  `Produce: readyToMerge (PRs green + only needing review/merge); conflictHotspots (files edited by >1 ref, ranked, naming refs); duplicateLanes (refs that are the same work to consolidate under one owner); stalePrunable (branches + lane paths safe to remove, with any locked-but-dead-pid lanes flagged for salvage-first); nextWave (3–5 dependency-ready items mapped to disjoint territories, ready to ASSIGN); coordinationRisks. Be concrete — name files, refs, territories. This feeds the Conductor PLAN/ASSIGN steps.`,
  { label: 'synthesis', phase: 'Synthesize', schema: { type: 'object', additionalProperties: false,
    required: ['readyToMerge', 'conflictHotspots', 'duplicateLanes', 'stalePrunable', 'nextWave', 'coordinationRisks'],
    properties: {
      readyToMerge: STR_ARR,
      conflictHotspots: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['file', 'refs', 'severity'], properties: { file: { type: 'string' }, refs: STR_ARR, severity: { type: 'string' } } } },
      duplicateLanes: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['topic', 'refs', 'recommendation'], properties: { topic: { type: 'string' }, refs: STR_ARR, recommendation: { type: 'string' } } } },
      stalePrunable: { type: 'object', additionalProperties: false, required: ['branches', 'lanes'], properties: { branches: STR_ARR, lanes: STR_ARR } },
      nextWave: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['item', 'territory'], properties: { item: { type: 'string' }, territory: { type: 'string' } } } },
      coordinationRisks: STR_ARR,
    } } })

return { lanes, branches, prs, collisions, gov, synthesis }
