// cleanup-sweep — read-only triage of branch/worktree sprawl into exact, verified
// prune/remove/salvage lists for the Conductor to execute. Never deletes anything
// itself (deletion is the orchestrator's accountable action). Usage:
//   Workflow({ name: 'cleanup-sweep' })
export const meta = {
  name: 'cleanup-sweep',
  description: 'Read-only triage of git branch + worktree sprawl: classify every ref as prune (merged / superseded), keep (open PR / real un-landed work → PR-candidate), or salvage (orphaned locked lane with a dead owning pid). Returns exact lists for the orchestrator to act on.',
  whenToUse: 'Periodically, or after a merge wave: lanes and build dirs are disposable, so keep branch/worktree count bounded without losing un-landed work.',
  phases: [
    { title: 'Classify', detail: 'parallel: merged-branch set, unmerged-branch triage, worktree state, large-lane delta' },
    { title: 'Consolidate', detail: 'one verified prune/keep/salvage plan' },
  ],
}

const STR_ARR = { type: 'array', items: { type: 'string' } }
const obj = (required, properties) => ({ type: 'object', additionalProperties: false, required, properties })

const MERGED_SCHEMA = obj(['mergedAncestors', 'caveat'], {
  mergedAncestors: STR_ARR,
  caveat: { type: 'string' },
})

const UNMERGED_SCHEMA = obj(['triage', 'pruneList'], {
  triage: { type: 'array', items: obj(['branch', 'recommendation', 'reason'], {
    branch: { type: 'string' }, recommendation: { type: 'string' }, reason: { type: 'string' },
  }) },
  pruneList: STR_ARR,
})

const WORKTREES_SCHEMA = obj(['worktrees'], {
  worktrees: { type: 'array', items: obj(['path', 'branch', 'ahead', 'dirty', 'locked', 'classification', 'reason'], {
    path: { type: 'string' }, branch: { type: 'string' }, ahead: { type: 'integer' },
    dirty: { type: 'boolean' }, locked: { type: 'boolean' },
    classification: { type: 'string' }, reason: { type: 'string' },
  }) },
})

const LARGE_SCHEMA = obj(['lanes', 'rescueFiles'], {
  lanes: { type: 'array', items: obj(['lane', 'trueUnlandedCommits', 'recommendation', 'reason'], {
    lane: { type: 'string' }, trueUnlandedCommits: { type: 'integer' },
    recommendation: { type: 'string' }, reason: { type: 'string' },
  }) },
  rescueFiles: STR_ARR,
})

const PLAN_SCHEMA = obj(['rescueFirst', 'removeWorktrees', 'salvageThenRemove', 'deleteBranches', 'prCandidates', 'consolidations'], {
  rescueFirst: STR_ARR,
  removeWorktrees: STR_ARR,
  salvageThenRemove: { type: 'array', items: obj(['path', 'salvageBranch'], {
    path: { type: 'string' }, salvageBranch: { type: 'string' },
  }) },
  deleteBranches: STR_ARR,
  prCandidates: { type: 'array', items: obj(['ref', 'scope'], {
    ref: { type: 'string' }, scope: { type: 'string' },
  }) },
  consolidations: { type: 'array', items: obj(['keep', 'absorbs'], {
    keep: { type: 'string' }, absorbs: STR_ARR,
  }) },
})

phase('Classify')
const [merged, unmerged, worktrees, largeLanes] = await parallel([
  () => agent(
    `READ-ONLY. In the Multiview repo (run git from the current checkout — branch refs are shared across every worktree, so never hard-code a repo path), list branches that are TRUE ancestors of origin/main and safe to delete with \`git branch -d\`: \`git branch --merged origin/main\` minus 'main' and minus any 'salvage/*'. Caveat: if the repo squash-merges, a landed branch may NOT show as merged — note that the orchestrator must verify non-ancestors with \`git cherry origin/main <br>\` or \`gh pr list --state merged --search head:<br>\` before deleting. Return the merged-ancestor list.`,
    { label: 'merged', phase: 'Classify', schema: MERGED_SCHEMA }),
  () => agent(
    `READ-ONLY. Triage branches NOT ancestors of origin/main in the Multiview repo (\`git branch --no-merged origin/main\` from the current checkout), EXCLUDING open-PR branches and salvage/* (keep those). For each: ahead count, whether its unique commits are real non-merge work or merge-train artifacts already equivalent in main (\`git cherry -v origin/main <br>\` — '-' lines are already applied), tip date. Recommend PRUNE (no unique un-landed work) or KEEP-PR-CANDIDATE (real work to rebase+land). Return {branch, recommendation, reason} per branch and a flat PRUNE list.`,
    { label: 'unmerged', phase: 'Classify', schema: UNMERGED_SCHEMA }),
  () => agent(
    `READ-ONLY. \`git worktree list --porcelain\` from the current checkout — it reports every lane of the Multiview repo whichever one you are in. For each lane: ahead of origin/main, dirty?, locked?. A 'locked' lane whose owning session pid is dead is orphaned, not protected — report locked + dirty + ahead and let the orchestrator confirm pid liveness. Classify each: REMOVE-NOW (ahead 0, clean, ancestor of main), SALVAGE-THEN-REMOVE (locked/dirty with un-committed or un-landed work), or KEEP (large in-flight, needs delta triage). Return {path, branch, ahead, dirty, locked, classification, reason}.`,
    { label: 'worktrees', phase: 'Classify', schema: WORKTREES_SCHEMA }),
  () => agent(
    `READ-ONLY. For each worktree lane of the Multiview repo >10 commits ahead of origin/main, compute the TRUE un-landed delta: \`git -C <lane> log --no-merges --cherry-pick --right-only origin/main...HEAD\` (excludes commits already patch-equivalent in main). Summarize the real un-landed work, whether the lane is a subset/superset of another (consolidation), whether it is on a stale base (rebase before PR), and which hot files it touches. Recommend PR-CANDIDATE (scope) / CONSOLIDATE-with-<lane> / PRUNE-already-landed / NEEDS-REBASE-then-PR. Flag any untracked file lost on removal (rescue first).`,
    { label: 'large-lanes', phase: 'Classify', schema: LARGE_SCHEMA }),
])

phase('Consolidate')
const plan = await agent(
  `Consolidate this cleanup triage into one verified action plan for the orchestrator to execute (it does the deletions).\n\nMERGED:\n${JSON.stringify(merged)}\n\nUNMERGED:\n${JSON.stringify(unmerged)}\n\nWORKTREES:\n${JSON.stringify(worktrees)}\n\nLARGE_LANES:\n${JSON.stringify(largeLanes)}\n\n` +
  `Return: rescueFirst (untracked files to copy out before any removal — data-loss risk); removeWorktrees (paths safe to remove now); salvageThenRemove (locked/dirty lanes: path + the salvage branch name to create); deleteBranches (verified prunable, '-d' safe set); prCandidates (refs with real un-landed work to rebase+land, with scope); consolidations (lane → absorbs which other lanes). Be conservative: never put a ref with unique un-landed work in deleteBranches.`,
  { label: 'plan', phase: 'Consolidate', schema: PLAN_SCHEMA })

return { merged, unmerged, worktrees, largeLanes, plan }
