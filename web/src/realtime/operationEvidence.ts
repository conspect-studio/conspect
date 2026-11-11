import type { Envelope } from './envelope';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

/** Validated output-tick evidence; the shared operation tracker owns its verdict. */
export interface SwitcherOperationEvidence {
  readonly corr: string;
  readonly topic: 'switcher';
  readonly resource: string;
  readonly event: 'switcher.operation';
  readonly me: string;
  readonly verdict: 'applied' | 'failed';
  readonly reason?: string;
}

/** Never infer a command outcome from a corr alone or from progress/loading. */
export function switcherOperationEvidence(envelope: Envelope): SwitcherOperationEvidence | undefined {
  if (envelope.t !== 'switcher.operation' || envelope.topic !== 'switcher' ||
      typeof envelope.id !== 'string' || !envelope.id ||
      typeof envelope.corr !== 'string' || !envelope.corr ||
      !isRecord(envelope.data)) return undefined;
  const data = envelope.data;
  if (typeof data.me !== 'string' || !data.me || data.me !== envelope.id ||
      data.operation_id !== envelope.corr || typeof data.program !== 'string' ||
      typeof data.tick !== 'number' || !Number.isFinite(data.tick) ||
      typeof data.completed !== 'boolean') return undefined;
  return { corr: envelope.corr, topic: 'switcher', resource: envelope.id,
    event: 'switcher.operation', me: data.me,
    verdict: data.completed ? 'applied' : 'failed',
    ...(!data.completed && typeof data.error === 'string' && data.error !== '' ? { reason: data.error } : {}) };
}
