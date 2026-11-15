// Terminal evidence for a patch take (ADR-0110 §12.8, #308), normalized per
// request path. `POST /patch/take` answers 202 and settles on exactly one
// event family, decided by the request (OpenAPI `PatchAccepted`):
//   * a set (2+ outputs, or mixer inputs with an output) and every patch
//     salvo → `patch.take`, one delivery per destination;
//   * one output → `route.output`;
//   * mixer inputs alone → `switcher.operation`.
// An event can arrive before the 202 sets the page's operation id, so
// unmatched evidence waits in a small bounded buffer (last 64, short TTL) and
// is reconciled on acceptance. Nothing here infers an outcome from a corr
// alone, or from an event family the request does not settle on.
import type { Envelope } from '../../realtime/envelope';
import { switcherOperationEvidence } from '../../realtime/operationEvidence';
import type { TakePath } from './patchMatrixModel';

/** One validated terminal delivery for an operation id. */
export interface TakeEvidence {
  readonly operationId: string;
  readonly path: TakePath;
  readonly outcome: 'applied' | 'refused';
  /** The server's typed reason when refused (`refusal`, `refused:*` outcome, or switcher `error`). */
  readonly reason?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Validate one envelope as patch-take terminal evidence, or `undefined`. */
export function takeEvidenceOf(envelope: Envelope): TakeEvidence | undefined {
  if (envelope.t === 'patch.take') {
    const data = envelope.data;
    if (!isRecord(data) || typeof data.operation_id !== 'string' || data.operation_id === '') return undefined;
    if (data.outcome !== 'applied' && data.outcome !== 'refused') return undefined;
    if (!isRecord(data.destination)) return undefined;
    return {
      operationId: data.operation_id,
      path: 'set',
      outcome: data.outcome,
      ...(data.outcome === 'refused' && typeof data.refusal === 'string' ? { reason: data.refusal } : {}),
    };
  }
  if (envelope.t === 'route.output') {
    const data = envelope.data;
    if (!isRecord(data) || typeof data.operation_id !== 'string' || data.operation_id === '') return undefined;
    if (typeof data.outcome !== 'string' || typeof data.output !== 'string') return undefined;
    return data.outcome === 'applied'
      ? { operationId: data.operation_id, path: 'output', outcome: 'applied' }
      : { operationId: data.operation_id, path: 'output', outcome: 'refused', reason: data.outcome };
  }
  const switcher = switcherOperationEvidence(envelope);
  if (switcher !== undefined) {
    return {
      operationId: switcher.corr,
      path: 'mixer',
      outcome: switcher.verdict === 'applied' ? 'applied' : 'refused',
      ...(switcher.reason !== undefined ? { reason: switcher.reason } : {}),
    };
  }
  return undefined;
}

/** Merge one more delivery into a settled outcome: a set is refused if any destination refused. */
export function mergeEvidence(current: TakeEvidence | undefined, next: TakeEvidence): TakeEvidence {
  if (current === undefined) return next;
  if (current.outcome === 'refused') return current;
  return next.outcome === 'refused' ? next : current;
}

export const EARLY_EVIDENCE_MAX = 64;
export const EARLY_EVIDENCE_TTL_MS = 10_000;

/** Bounded early-evidence buffer: last {@link EARLY_EVIDENCE_MAX} deliveries, each kept {@link EARLY_EVIDENCE_TTL_MS}. */
export class EarlyEvidence {
  private items: { readonly evidence: TakeEvidence; readonly at: number }[] = [];

  constructor(
    private readonly max = EARLY_EVIDENCE_MAX,
    private readonly ttlMs = EARLY_EVIDENCE_TTL_MS,
  ) {}

  push(evidence: TakeEvidence, now: number): void {
    this.items = this.items.filter((item) => now - item.at <= this.ttlMs);
    this.items.push({ evidence, at: now });
    while (this.items.length > this.max) this.items.shift();
  }

  /** Remove and return every live delivery for `operationId` on the expected `path`, in arrival order. */
  drain(operationId: string, path: TakePath, now: number): readonly TakeEvidence[] {
    const live = this.items.filter((item) => now - item.at <= this.ttlMs);
    const hits = live.filter((item) => item.evidence.operationId === operationId && item.evidence.path === path);
    this.items = live.filter((item) => !hits.includes(item));
    return hits.map((item) => item.evidence);
  }

  get size(): number {
    return this.items.length;
  }
}
