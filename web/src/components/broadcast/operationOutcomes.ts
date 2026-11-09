// operationOutcomes — the pure, bounded store that joins REST 202 command
// acceptances to their realtime TERMINAL evidence (the "202 accepted ≠
// applied" gap; production component audit §3). Pages today toast the
// `operation_id` at 202 and nothing reconciles it with the outcome frames the
// engine streams later; this module is the sanctioned join point.
//
// Truth rules (pinned by operationOutcomes.test.ts):
// - EXACT evidence: a frame settles an acceptance only when the correlation
//   id (`corr`) and the realtime `topic` match exactly, plus the resource id
//   and the event type when the expectation pins them. A same-corr frame on
//   the wrong topic or naming the wrong resource settles nothing.
// - `accepted` can only ever become `applied`, `failed` or `unknown`. There
//   is deliberately NO "rolled back" phase: an outcome that was never
//   observed never implies the engine reverted (the server's bounded corr
//   registry is a drop-oldest FIFO with no expiry event, so no rollback
//   guarantee exists — docs/api/realtime.md §"corr").
// - `job.progress` is NEVER terminal evidence (the AsyncAPI models progress
//   only; the advertised job.accepted/result/failed lifecycle does not
//   exist). Progress frames settle nothing and are never cached.
// - Uncorrelated frames (no `corr`) can never produce success — the corr is
//   the only join key, so deliberately-uncorrelated server commands (swap,
//   route, apply-layout, …) are outside this store by construction, and an
//   acceptance without a corr is refused.
// - A deadline only ever yields `unknown` (outcome not observed), never a
//   fake `failed`; an acceptance with NO deadline never ages out on its own.
// - The terminal-before-acceptance race is served by a BOUNDED early-evidence
//   cache (drop-oldest). Eviction is safe: a dropped frame simply leaves the
//   later acceptance `accepted` (→ `unknown` at its deadline) — never a
//   fabricated outcome.
//
// Non-goals, on purpose: no fetch, no `localStorage`, no timers — the caller
// owns the clock and drives every transition, so the store stays pure. No
// replay/retry POST and no HTTP reconcile endpoint (none exists server-side).
// The module deliberately does NOT import the generated realtime schema:
// wiring it into `useEngineEvents` waits for the reconciled AsyncAPI schema
// (the missing `idle` OutputRunState drift, audit §2) plus review; until then
// the wiring layer maps envelopes onto the plain shapes below.

/** The phases one tracked operation can be in (first terminal verdict wins). */
export type OperationPhase = 'accepted' | 'applied' | 'failed' | 'unknown';

/** The terminal verdict a piece of evidence carries. */
export type EvidenceVerdict = 'applied' | 'failed';

/** Event types that are evidence of NOTHING terminal (progress is not an outcome). */
export const NON_TERMINAL_EVENTS: ReadonlySet<string> = new Set(['job.progress']);

/** Max terminal-evidence frames cached ahead of their acceptance (drop-oldest). */
export const EARLY_EVIDENCE_MAX = 64;

/**
 * Max tracked-operation records kept. Settled/expired history is evicted
 * oldest-first beyond this bound; UNRESOLVED (`accepted`) records are never
 * evicted — dropping a live operation would fabricate "no such operation",
 * so a burst larger than the cap of concurrently-unresolved ops grows past it.
 */
export const OPERATION_RECORDS_MAX = 256;

/** One accepted operation's expectation of its terminal evidence. */
export interface OperationAcceptance {
  /** The `202 Accepted` body's `operation_id`. */
  readonly operationId: string;
  /** The correlation id echoed on the outcome frame (exact match required). */
  readonly corr: string;
  /** The realtime topic terminal evidence must arrive on (exact match). */
  readonly topic: string;
  /** The resource id the evidence must name, when the command is scoped to one. */
  readonly resource?: string | undefined;
  /** The event type the evidence must carry, when the caller can pin it. */
  readonly event?: string | undefined;
  /** The command kind from the acceptance body (for caller-side copy). */
  readonly kind?: string | undefined;
  /** Wall-clock ms the 202 was observed at, when the caller tracks it. */
  readonly acceptedAt?: number | undefined;
  /**
   * Wall-clock ms after which the outcome is reported `unknown` (never
   * `failed`). Absent = the caller set no deadline; the record never ages
   * out on its own.
   */
  readonly deadlineAt?: number | undefined;
}

/** A realtime frame offered as terminal evidence for an acceptance. */
export interface TerminalEvidence {
  /** The echoed correlation id; empty means uncorrelated and settles nothing. */
  readonly corr: string;
  /** The topic the frame arrived on. */
  readonly topic: string;
  /** The frame's event type (`t`); `job.progress` is never terminal. */
  readonly event?: string | undefined;
  /** The resource the frame names (envelope `id`), when it carries one. */
  readonly resource?: string | undefined;
  /** The terminal verdict the caller's envelope mapping derived from the frame. */
  readonly verdict: EvidenceVerdict;
  /** Wall-clock ms the frame was observed at, when the caller tracks it. */
  readonly at?: number | undefined;
}

/** One tracked operation: its expectation plus the phase it settled to. */
export interface OperationRecord {
  readonly operationId: string;
  readonly corr: string;
  readonly topic: string;
  readonly resource?: string | undefined;
  readonly event?: string | undefined;
  readonly kind?: string | undefined;
  readonly phase: OperationPhase;
  readonly acceptedAt?: number | undefined;
  readonly deadlineAt?: number | undefined;
  /** Wall-clock ms of the terminal evidence that settled the record, when known. */
  readonly settledAt?: number | undefined;
}

/** The store: tracked operation records + the bounded early-evidence cache. */
export interface OperationOutcomeState {
  readonly records: ReadonlyMap<string, OperationRecord>;
  readonly earlyEvidence: readonly TerminalEvidence[];
}

/** An empty store. */
export function createOperationOutcomeState(): OperationOutcomeState {
  return { records: new Map(), earlyEvidence: [] };
}

/**
 * Record a `202 Accepted` operation and what its terminal evidence must look
 * like. Idempotent per `operationId`: a replayed acceptance never regresses a
 * settled or deadline-expired record, and while the record is still waiting
 * for evidence the replay may only re-arm its deadline. If matching terminal
 * evidence was already cached (it arrived before the 202), the record settles
 * immediately and the cached frame is consumed.
 */
export function applyOperationAcceptance(
  state: OperationOutcomeState,
  acceptance: OperationAcceptance,
): OperationOutcomeState {
  // An operation without a corr can never be joined to evidence, and an
  // id-less acceptance cannot be tracked; both are refused rather than
  // fabricated.
  if (
    acceptance.operationId === '' ||
    acceptance.corr === '' ||
    acceptance.topic === ''
  ) {
    return state;
  }
  const existing = state.records.get(acceptance.operationId);
  if (existing !== undefined) {
    if (existing.phase !== 'accepted') {
      // Settled or deadline-expired: the replay changes nothing.
      return state;
    }
    if (
      acceptance.deadlineAt === undefined ||
      acceptance.deadlineAt === existing.deadlineAt
    ) {
      return state;
    }
    const replayed = new Map(state.records);
    replayed.set(acceptance.operationId, {
      ...existing,
      deadlineAt: acceptance.deadlineAt,
    });
    return { records: replayed, earlyEvidence: state.earlyEvidence };
  }

  const record: OperationRecord = {
    operationId: acceptance.operationId,
    corr: acceptance.corr,
    topic: acceptance.topic,
    phase: 'accepted',
    resource: acceptance.resource,
    event: acceptance.event,
    kind: acceptance.kind,
    acceptedAt: acceptance.acceptedAt,
    deadlineAt: acceptance.deadlineAt,
  };

  // Terminal-before-acceptance race: consume the FIRST matching cached frame
  // (earliest terminal evidence wins, matching post-acceptance behaviour).
  const evidenceIndex = state.earlyEvidence.findIndex((cached) =>
    matchesExpectation(record, cached),
  );
  const joined =
    evidenceIndex >= 0 ? state.earlyEvidence[evidenceIndex] : undefined;
  const records = new Map(state.records);
  if (joined !== undefined) {
    records.set(acceptance.operationId, settleRecord(record, joined));
    const earlyEvidence = state.earlyEvidence.filter(
      (_, index) => index !== evidenceIndex,
    );
    return {
      records: evictSettled(records, OPERATION_RECORDS_MAX),
      earlyEvidence,
    };
  }
  records.set(acceptance.operationId, record);
  return {
    records: evictSettled(records, OPERATION_RECORDS_MAX),
    earlyEvidence: state.earlyEvidence,
  };
}

/**
 * Offer a realtime frame as terminal evidence. Frames with exact corr+topic
 * (+pinned resource/event) settle every waiting record with the frame's
 * verdict; a frame that matches an already-settled record is late noise (the
 * first terminal verdict stands) and is dropped; an unmatched frame is cached
 * for the acceptance that may follow. `job.progress` and uncorrelated frames
 * settle nothing and are never cached.
 */
export function applyTerminalEvidence(
  state: OperationOutcomeState,
  evidence: TerminalEvidence,
): OperationOutcomeState {
  if (evidence.corr === '') {
    // Uncorrelated frames can never produce success.
    return state;
  }
  if (evidence.event !== undefined && NON_TERMINAL_EVENTS.has(evidence.event)) {
    // Progress is not an outcome: it settles nothing and is not evidence.
    return state;
  }
  const records = new Map(state.records);
  let settledAny = false;
  for (const [operationId, record] of state.records) {
    // `unknown` records stay reconcilable: terminal evidence arriving after a
    // deadline (the caller's reconciliation) may still settle them honestly.
    if (
      (record.phase === 'accepted' || record.phase === 'unknown') &&
      matchesExpectation(record, evidence)
    ) {
      records.set(operationId, settleRecord(record, evidence));
      settledAny = true;
    }
  }
  if (settledAny) {
    return { records, earlyEvidence: state.earlyEvidence };
  }
  for (const record of state.records.values()) {
    if (
      (record.phase === 'applied' || record.phase === 'failed') &&
      matchesExpectation(record, evidence)
    ) {
      // First terminal verdict stands; later conflicting evidence is noise.
      return state;
    }
  }
  if (state.earlyEvidence.some((cached) => sameEvidence(cached, evidence))) {
    return state;
  }
  const grown = [...state.earlyEvidence, evidence];
  const overflow = grown.length - EARLY_EVIDENCE_MAX;
  // Drop-oldest eviction is safe: the later acceptance simply finds no frame
  // and stays `accepted` (→ `unknown` at its deadline) — never a fabrication.
  return {
    records: state.records,
    earlyEvidence: overflow > 0 ? grown.slice(overflow) : grown,
  };
}

/**
 * Advance the store's clock: every `accepted` record whose deadline has
 * passed becomes `unknown` (outcome not observed) — never a fake `failed`
 * and never a rollback claim. Records without a deadline never age out.
 */
export function expireOperationDeadlines(
  state: OperationOutcomeState,
  nowMs: number,
): OperationOutcomeState {
  const records = new Map<string, OperationRecord>();
  let expired = false;
  for (const [operationId, record] of state.records) {
    if (
      record.phase === 'accepted' &&
      record.deadlineAt !== undefined &&
      record.deadlineAt <= nowMs
    ) {
      records.set(operationId, { ...record, phase: 'unknown' });
      expired = true;
    } else {
      records.set(operationId, record);
    }
  }
  return expired ? { records, earlyEvidence: state.earlyEvidence } : state;
}

/** Read one tracked operation by its `operation_id`, if the store holds it. */
export function readOperationOutcome(
  state: OperationOutcomeState,
  operationId: string,
): OperationRecord | undefined {
  return state.records.get(operationId);
}

/** Whether a frame satisfies a record's expectation: exact corr + topic, plus
 * the pinned resource/event when the expectation names them. */
function matchesExpectation(
  record: Pick<
    OperationRecord,
    'corr' | 'topic' | 'resource' | 'event'
  >,
  evidence: TerminalEvidence,
): boolean {
  if (record.corr !== evidence.corr) {
    return false;
  }
  if (record.topic !== evidence.topic) {
    return false;
  }
  if (record.resource !== undefined && evidence.resource !== record.resource) {
    return false;
  }
  if (record.event !== undefined && evidence.event !== record.event) {
    return false;
  }
  return true;
}

/** Apply a terminal verdict to a record, stamping the evidence time when known. */
function settleRecord(
  record: OperationRecord,
  evidence: TerminalEvidence,
): OperationRecord {
  return evidence.at === undefined
    ? { ...record, phase: evidence.verdict }
    : { ...record, phase: evidence.verdict, settledAt: evidence.at };
}

/** Whether two evidence frames are identical in every field (cache dedupe). */
function sameEvidence(a: TerminalEvidence, b: TerminalEvidence): boolean {
  return (
    a.corr === b.corr &&
    a.topic === b.topic &&
    a.event === b.event &&
    a.resource === b.resource &&
    a.verdict === b.verdict &&
    a.at === b.at
  );
}

/**
 * Enforce the records bound by dropping the OLDEST settled/expired records;
 * unresolved (`accepted`) records are never evicted. Iteration follows Map
 * insertion order (acceptance order), so "oldest" is well-defined.
 */
function evictSettled(
  records: ReadonlyMap<string, OperationRecord>,
  max: number,
): ReadonlyMap<string, OperationRecord> {
  let overflow = records.size - max;
  if (overflow <= 0) {
    return records;
  }
  const next = new Map(records);
  for (const [operationId, record] of records) {
    if (overflow <= 0) {
      break;
    }
    if (record.phase !== 'accepted') {
      next.delete(operationId);
      overflow -= 1;
    }
  }
  return next;
}
