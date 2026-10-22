// review-wave — adversarial cross-vendor review of one or more diffs/PRs: the
// mandatory pre-merge gate, never self-performed by the authoring vendor.
// Each Claude-authored diff is reviewed by a DIFFERENT vendor (Codex) in a fresh
// context seeing only the diff + spec + checklist. High-risk items get a 3-reviewer
// panel (Codex + two fresh-context lenses). Returns structured verdicts; the
// orchestrator gates merge on them.
// CLASS GATING BELONGS TO THE CALLER, not to this script: an R0 (prose-only) item
// owes no review — do not dispatch it here; R1/R2 get a single pass, R3 sets
// highRisk: true for the 3-lens panel. Usage:
//   Workflow({ name: 'review-wave', args: { items: [
//     { id: 'pr-170', ref: '170', spec: 'webrtc ICE concrete local addr; invariant #10 isolation', highRisk: false },
//     { id: 'pr-172', ref: '172', spec: 'output-clock cadence-hold; invariant #1', highRisk: true } ] } })
// ref = a PR number (reviewed via `gh pr diff <n>`) or a git ref (`git diff origin/main...<ref>`).
export const meta = {
  name: 'review-wave',
  description: 'Adversarial cross-vendor review: dispatch each Claude-authored diff to Codex (a different vendor) in a fresh context with only diff+spec+checklist; high-risk diffs get a 3-reviewer panel. Returns per-item verdicts (defects + >=1 residual risk + blocked?) for the orchestrator to gate merge on.',
  whenToUse: 'Before merging any R1+ lane/PR — the mandatory review gate (ADR-G005). Never self-performed by the authoring vendor.',
  phases: [{ title: 'Review' }],
}

const VERDICT = { type: 'object', additionalProperties: false,
  required: ['id', 'reviewer', 'ranOk', 'defects', 'highestResidualRisk', 'blocked'],
  properties: {
    id: { type: 'string' }, reviewer: { type: 'string', description: 'codex | codex+panel | claude-fallback — the vendor whose judgment this is' },
    ranOk: { type: 'boolean', description: 'false if the reviewer CLI could not be run; then this is a labeled fallback' },
    defects: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['severity', 'where', 'what'],
      properties: { severity: { type: 'string', description: 'blocker | major | minor' }, where: { type: 'string', description: 'file:line' }, what: { type: 'string' } } } },
    highestResidualRisk: { type: 'string', description: 'the single highest-residual-risk area — required even when no defect is found; unanimous bland approval is itself a yellow flag' },
    blocked: { type: 'boolean', description: 'true if any blocker/major defect must be fixed before merge' },
  } }

const CHECKLIST = 'Scope (docs/standards/engineering.md Part D, evidence rules): correctness, security, spec-conformance, and the typing/TDD guardrails ONLY. NOT style/naming/speculative hardening. A finding blocks the merge only with a reproduction, a failing test, or a cited line demonstrably violating a stated invariant — an unreproducible finding is advisory, so report it and say which it is. Multiview invariants to weigh: #1 output-clock (one valid frame per tick, inputs never pace), #10 isolation (control/preview/realtime physically cannot back-pressure the engine), bounded drop-oldest queues, no unwrap/panic on the hot path, FFI safety. Report concrete defects as file:line + severity; if none, name the single highest-residual-risk area. Unanimous bland approval is a yellow flag — always surface >=1 substantive risk.'

function reviewItem(it) {
  const single = (lensNote, idSuffix) => agent(
    `You are the harness for a CROSS-VENDOR adversarial code review. The authoring vendor is Claude; the REVIEWER must be a DIFFERENT vendor — the \`codex\` CLI (codex-cli, installed). Do NOT substitute your own Claude judgment for the verdict; run Codex and relay ITS findings faithfully.\n\n` +
    `Steps (run these from anywhere inside the Multiview repo checkout — both git and gh need to be inside it; never hard-code a repo path):\n` +
    `1. Build the diff. If ref "${it.ref}" is all digits it is a PR: \`gh pr diff ${it.ref} > /tmp/rv-${it.id}${idSuffix}.diff\`. Else: \`git diff origin/main...${it.ref} > /tmp/rv-${it.id}${idSuffix}.diff\`.\n` +
    `2. Discover the right non-interactive Codex invocation: \`codex --help\` then \`codex exec --help\` (or \`codex e --help\`). Use the read-only/non-interactive exec form (e.g. \`codex exec --sandbox read-only "<prompt>"\`). The prompt must tell Codex to read /tmp/rv-${it.id}${idSuffix}.diff and review it.\n` +
    `3. Prompt Codex: "Adversarially review this diff. Spec: ${it.spec}. ${lensNote} ${CHECKLIST}". Capture Codex's full output (give it a generous timeout).\n` +
    `4. If Codex ran, set reviewer="codex"${idSuffix ? ' (lens: ' + lensNote + ')' : ''}, ranOk=true, and STRUCTURE CODEX'S findings into the verdict (do not invent defects Codex did not raise; do not drop ones it did). If Codex could NOT be run after a genuine attempt (report the error), fall back to performing the review yourself in this fresh context, set reviewer="claude-fallback", ranOk=false, set blocked=true (a fallback is not cross-vendor and must fail closed), and note the error in highestResidualRisk.\n` +
    `Return the structured verdict for id "${it.id}${idSuffix}".`,
    { label: `review:${it.id}${idSuffix}`, phase: 'Review', schema: VERDICT })

  if (!it.highRisk) return single('', '')
  // High-risk: a fixed N-reviewer panel — Codex + fresh-context lenses — then aggregate.
  const lenses = [
    () => single('Lens: correctness & concurrency/TOCTOU/race/timing.', '-a'),
    () => single('Lens: security & authorization (BOLA/per-object authz) & input hardening.', '-b'),
    () => single('Lens: spec-conformance & the Multiview invariants (#1 output-clock, #10 isolation) & data-plane bounded-queue discipline.', '-c'),
  ]
  return parallel(lenses).then((panel) => {
    // Deterministic code-aggregation, FAIL-CLOSED. An LLM aggregator over the embedded panel
    // JSON is fragile — its StructuredOutput call can fail to parse and lose the whole verdict.
    // A defect from ANY lens stands; a MISSING/failed lens (a falsy panel entry — a lens that
    // died or emitted unparseable output — or one with ranOk=false) means the panel is
    // INCOMPLETE: ranOk=false AND blocked=true, never a clear verdict from the survivors alone.
    // Also blocked if any present lens blocked or raised a blocker/major.
    const vs = panel.filter(Boolean)
    // Fail closed by EXPECTED INDEX, not by iterating present elements: check every expected
    // lens slot directly (Array.every/filter SKIP holes, so a sparse array could otherwise pass).
    // A hole, null, undefined, short array, or a present-but-malformed verdict (ranOk!==true) at
    // any lens index fails the check.
    const allLensesRan = panel.length === lenses.length && lenses.every((_, i) => panel[i] != null && panel[i].ranOk === true)
    const defects = vs.flatMap((x) => x.defects || [])
    const blocked = !allLensesRan || vs.some((x) => x.blocked) || defects.some((d) => d.severity === 'blocker' || d.severity === 'major')
    return {
      id: it.id,
      reviewer: 'codex+panel',
      ranOk: allLensesRan,
      defects,
      highestResidualRisk: vs.map((x) => x.highestResidualRisk).filter(Boolean).join('  ||  ') || 'panel produced no residual-risk statement',
      blocked,
    }
  })
}

phase('Review')
const items = (args && args.items) || []
const verdicts = await pipeline(items, reviewItem)
// Fail closed at TWO levels, keyed to each submitted item by index:
// (1) a null item result — the whole review died/dropped — becomes an EXPLICIT blocked verdict
//     so it can never silently vanish from the gate (the caller gets one verdict per item); and
// (2) a fallback verdict (ranOk=false → not actually cross-vendor) can never clear the gate, so
//     couple blocked to ranOk (codex-review runbook + orchestrate skill: never merge on fallback).
const enforced = items.map((it, i) => {
  const v = verdicts[i]
  if (!v) {
    return {
      id: it.id,
      reviewer: 'none',
      ranOk: false,
      defects: [{ severity: 'blocker', where: 'review-wave', what: 'the reviewer produced no verdict (review failed or was dropped) — failing closed' }],
      highestResidualRisk: 'review did not complete',
      blocked: true,
    }
  }
  // Fail closed: ONLY an explicit ranOk===true may pass; anything else (false, or a malformed
  // verdict that omits ranOk) is forced blocked.
  return v.ranOk === true ? v : { ...v, blocked: true }
})
return { verdicts: enforced }
