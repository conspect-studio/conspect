// operationOutcomes — the pure, hard-capped store that joins REST 202 command
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
// - Reserve a capacity seat BEFORE submitting; a reserved command becomes
//   accepted on REST 202. A 202 without reservation may fail admission with
//   an explicit capacity error: it has ALREADY been accepted by the engine,
//   so callers must retain/reconcile it outside this store, never say rejected.
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

/** The phases one tracked operation can be in (first valid terminal verdict wins). */
export type OperationPhase = 'reserved' | 'accepted' | 'applied' | 'failed' | 'unknown';

/** A rejected admission never asserts that an already-submitted engine command failed. */
export class OperationOutcomeError extends Error {
  constructor(readonly code: 'capacity' | 'identity' | 'invalid') {
    super(`Operation outcome ${code}: submission status is not inferred`);
    this.name = 'OperationOutcomeError';
  }
}

/** The terminal verdict a piece of evidence carries. */
export type EvidenceVerdict = 'applied' | 'failed';

/** Event types that are evidence of NOTHING terminal (progress is not an outcome). */
export const NON_TERMINAL_EVENTS: ReadonlySet<string> = new Set(['job.progress']);

/** Max terminal-evidence frames cached ahead of their acceptance (drop-oldest). */
export const EARLY_EVIDENCE_MAX = 64;

/**
 * Hard cap. Only authoritative applied/failed history may be evicted;
 * reserved, accepted and unknown operations retain their uncertain authority.
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

/** Reserve a seat before submitting a command. A capacity rejection means DO NOT POST.
 * Carry the returned state forward to the 202 acceptance. No I/O occurs here. */
export function reserveOperationOutcome(
  state: OperationOutcomeState,
  expectation: OperationAcceptance,
): OperationOutcomeState {
  validateAcceptance(expectation);
  if (state.records.has(expectation.operationId) ||
      [...state.records.values()].some((record) => record.corr === expectation.corr)) {
    throw new OperationOutcomeError('identity');
  }
  const records = makeRoom(state.records);
  records.set(expectation.operationId, makeRecord(expectation, 'reserved'));
  return { records, earlyEvidence: state.earlyEvidence };
}

/** An empty store. */
export function createOperationOutcomeState(): OperationOutcomeState {
  return { records: new Map(), earlyEvidence: [] };
}

/**
 * Record a `202 Accepted` operation. Prefer reserveOperationOutcome before
 * POST; absent a reservation, this may explicitly throw on full capacity.
 * A replay is valid only for the same identity; its deadline may extend,
 * never shorten. Cached earlier terminal evidence settles on acceptance.
 */
export function applyOperationAcceptance(
  state: OperationOutcomeState,
  acceptance: OperationAcceptance,
): OperationOutcomeState {
  validateAcceptance(acceptance);
  const existing = state.records.get(acceptance.operationId);
  if (existing !== undefined) {
    if (!sameIdentity(existing, acceptance)) throw new OperationOutcomeError('identity');
    if (existing.phase === 'reserved') {
      if (existing.deadlineAt !== undefined && acceptance.deadlineAt !== undefined &&
          existing.deadlineAt !== acceptance.deadlineAt) throw new OperationOutcomeError('identity');
      const records = new Map(state.records);
      const record = { ...existing, phase: 'accepted' as const,
        acceptedAt: acceptance.acceptedAt, deadlineAt: acceptance.deadlineAt };
      const joined = state.earlyEvidence.findIndex((item) => matchesExpectation(record, item));
      records.set(acceptance.operationId, joined < 0 ? record : settleRecord(record, state.earlyEvidence.slice(joined, joined + 1)[0] ?? evidenceFallback()));
      return { records, earlyEvidence: joined < 0 ? state.earlyEvidence :
        state.earlyEvidence.filter((_, index) => index !== joined) };
    }
    if (existing.phase !== 'accepted' || acceptance.deadlineAt === undefined ||
        (existing.deadlineAt !== undefined && acceptance.deadlineAt <= existing.deadlineAt)) return state;
    const records = new Map(state.records);
    records.set(acceptance.operationId, { ...existing, deadlineAt: acceptance.deadlineAt });
    return { records, earlyEvidence: state.earlyEvidence };
  }
  if ([...state.records.values()].some((record) => record.corr === acceptance.corr))
    throw new OperationOutcomeError('identity');
  // An unsolicited 202 has ALREADY happened. Throw an explicit capacity signal;
  // the caller must keep its acceptance outside this store for reconciliation.
  // Never return an unchanged state as if the engine declined the command.
  const records = makeRoom(state.records);
  const record = makeRecord(acceptance, 'accepted');
  const joined = state.earlyEvidence.findIndex((item) => matchesExpectation(record, item));
  records.set(acceptance.operationId, joined < 0 ? record : settleRecord(record, state.earlyEvidence.slice(joined, joined + 1)[0] ?? evidenceFallback()));
  return { records, earlyEvidence: joined < 0 ? state.earlyEvidence :
    state.earlyEvidence.filter((_, index) => index !== joined) };
}

function evidenceFallback(): TerminalEvidence {
  throw new OperationOutcomeError('invalid');
}
function validText(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0;
}
function validOptionalText(value: unknown): boolean {
  return value === undefined || validText(value);
}
function validOptionalTime(value: unknown): boolean {
  return value === undefined || (typeof value === 'number' && Number.isFinite(value));
}
function validateAcceptance(value: OperationAcceptance): void {
  if (!validText(value.operationId) || !validText(value.corr) ||
      !validText(value.topic) || !validOptionalText(value.resource) ||
      !validOptionalText(value.event) || !validOptionalText(value.kind) ||
      !validOptionalTime(value.acceptedAt) || !validOptionalTime(value.deadlineAt))
    throw new OperationOutcomeError('invalid');
}
function sameIdentity(a: OperationRecord, b: OperationAcceptance): boolean {
  return a.corr === b.corr && a.topic === b.topic && a.resource === b.resource &&
    a.event === b.event && a.kind === b.kind &&
    (a.acceptedAt === undefined || b.acceptedAt === undefined || a.acceptedAt === b.acceptedAt);
}
function makeRecord(value: OperationAcceptance, phase: 'reserved' | 'accepted'): OperationRecord {
  return { operationId: value.operationId, corr: value.corr, topic: value.topic,
    resource: value.resource, event: value.event, kind: value.kind,
    acceptedAt: value.acceptedAt, deadlineAt: value.deadlineAt, phase };
}
function makeRoom(current: ReadonlyMap<string, OperationRecord>): Map<string, OperationRecord> {
  const records = new Map(current);
  if (records.size < OPERATION_RECORDS_MAX) return records;
  for (const [id, record] of records) {
    if (record.phase === 'applied' || record.phase === 'failed') {
      records.delete(id);
      return records;
    }
  }
  throw new OperationOutcomeError('capacity');
}

/**
 * Offer a realtime frame as terminal evidence. Frames with exact corr+topic
 * (+pinned resource/event) settle the waiting record with the frame's
 * verdict; a frame that matches an already-settled record is late noise (the
 * first terminal verdict stands) and is dropped; an unmatched frame is cached
 * for the acceptance that may follow. `job.progress` and uncorrelated frames
 * settle nothing and are never cached.
 */
export function applyTerminalEvidence(
  state: OperationOutcomeState,
  evidence: TerminalEvidence,
): OperationOutcomeState {
  if (!validText(evidence.corr) || !validText(evidence.topic) ||
      !validOptionalText(evidence.resource) || !validOptionalText(evidence.event) ||
      !validOptionalTime(evidence.at) ||
      !(['applied', 'failed'] as readonly unknown[]).includes(evidence.verdict)) {
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
      matchesExpectation(record, evidence) &&
      ![...state.records.values()].some((other) => other.corr === record.corr &&
        (other.phase === 'applied' || other.phase === 'failed'))
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
  if (state.earlyEvidence.some((cached) => cached.corr === evidence.corr)) {
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
  if (!Number.isFinite(nowMs)) throw new OperationOutcomeError('invalid');
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

/** Retained history is evicted only on admission, never on expiry. */
